import {
  createToolRegistry,
  materializePromptMessages,
  registerBuiltInTools,
  registerInstalledTools,
  registerLegacyAgentToolsFromDirectoryIfExists,
  registerLibraryToolModulePath,
  turnMetadata,
  type BuiltInToolName,
  type EventData,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";
import {
  responseToLinguaEvents,
  responseToolCalls,
  runtimeFromModelBinding,
  type NativeResponsesRequest,
  type ResponsesRuntimeLike,
  type TraceParent,
} from "@exo/model-runtime/responses";
import { ensureTable } from "@exo/model-runtime/cost";

import { resolveLlmBinding } from "./shared";

export interface ResponsesTurnLoopOptions {
  instructions?: (context: TurnContext) => Message[] | Promise<Message[]>;
  registerTools?: (
    tools: HarnessToolRegistry,
    context: TurnContext,
  ) => Promise<void> | void;
}

export async function runResponsesHarnessTurn(
  context: TurnContext,
  options: ResponsesTurnLoopOptions = {},
): Promise<void> {
  await ensureTable(); // load the price table once so cost is ready when events are built
  const modelBinding = await resolveLlmBinding(context);
  const runtime = runtimeFromModelBinding(context.agentConfig, modelBinding);
  await runtime.runTurn(context, (turnParent) =>
    runResponsesTurnLoop(
      runtime,
      context,
      turnParent,
      modelBinding.model,
      options,
    ),
  );
}

export async function createDefaultToolRegistry(
  context: TurnContext,
  builtInToolNames: BuiltInToolName[] = defaultBuiltInToolNames(context),
): Promise<HarnessToolRegistry> {
  const tools = createToolRegistry(context);
  registerBuiltInTools(tools, context, builtInToolNames);
  for (const modulePath of context.agentConfig.typescript?.toolModulePaths ??
    []) {
    await registerLibraryToolModulePath(tools, context, modulePath);
  }
  await registerConfiguredAgentTools(tools, context);
  return tools;
}

export async function registerConfiguredAgentTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  await registerInstalledTools(tools, context);
  if (context.agentConfig.enableAgentToolCreation) {
    await registerLegacyAgentToolsFromDirectoryIfExists(tools, context);
  }
}

export function defaultBuiltInToolNames(
  context: TurnContext,
): BuiltInToolName[] {
  const names: BuiltInToolName[] = ["shell"];
  if (context.agentConfig.enableAgentToolCreation) {
    names.push("install_agent_tool", "uninstall_agent_tool");
  }
  return names;
}

export function basicHarnessInstructions(context: TurnContext): Message[] {
  return context.agentConfig.enableAgentToolCreation
    ? [...context.agentConfig.instructions, agentToolCreationInstruction()]
    : context.agentConfig.instructions;
}

export function agentToolCreationInstruction(): Message {
  return {
    role: "developer",
    content:
      "Agent-created tools are supported. When the user asks you to create a reusable tool, call install_agent_tool with a complete TypeScript moduleSource. Do not claim the tool was created unless install_agent_tool returns ok: true. The moduleSource must use type-only imports from @exo/harness/tool and default-export a Tool using { definition, initializationParameters, initialize(...) } satisfies Tool; definition.parameters must be a strict JSON schema object with additionalProperties: false; handlers must implement execute(args, execution), not invoke or call. Do not use zod, inputSchema, external npm packages, or runtime imports from @exo/harness/tool. After install_agent_tool succeeds, the new tool is available in the next model round of the same turn, so use it directly rather than falling back to shell. Use uninstall_agent_tool to remove an agent-created tool that is obsolete or conflicts with another tool name.",
  };
}

export async function runResponsesTurnLoop(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  turnParent: TraceParent,
  model: string,
  options: ResponsesTurnLoopOptions,
): Promise<string | null> {
  const { conversation } = context.exoharness.current;
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  let latestEventId: string | null = null;
  let emptyRetries = 0;
  const MAX_EMPTY_RETRIES = 2;
  // Duplicate-send guard: weak models can loop on send_adapter_message, blasting
  // the user with near-identical variants of the same reply. Track whether a
  // reply has already gone out with no real work since — a second send with
  // nothing done in between is a duplicate and is skipped.
  let repliedSinceWork = false;

  for (let round = 0; ; round += 1) {
    if (
      maxToolRoundTrips !== null &&
      maxToolRoundTrips !== undefined &&
      round > maxToolRoundTrips
    ) {
      return latestEventId;
    }

    const tools = options.registerTools
      ? createToolRegistry(context)
      : await createDefaultToolRegistry(context);
    if (options.registerTools) {
      await options.registerTools(tools, context);
    }
    const messages = await materializePromptMessages(
      conversation,
      options.instructions
        ? await options.instructions(context)
        : basicHarnessInstructions(context),
    );
    const request: NativeResponsesRequest = {
      model,
      messages,
      tools: tools.definitions(),
      maxOutputTokens: context.agentConfig.maxOutputTokens,
      metadata: turnMetadata(context),
    };

    const response = context.streaming
      ? await runtime.completeStream(
          request,
          {
            onFirstChunk: (ttftMs) => context.stream.firstChunk(ttftMs),
            onTextDelta: (text) => context.stream.text(text),
          },
          {
            parent: turnParent,
            roundIndex: round,
          },
        )
      : await runtime.complete(request, {
          parent: turnParent,
          roundIndex: round,
        });

    const events = responseToLinguaEvents(response);
    if (events.length > 0) {
      latestEventId = await appendTurnEvents(context, events);
    }

    const toolCalls = responseToolCalls(response);
    const hasSyntheticToolResult = events.some(
      (event) => event.type === "tool_result",
    );
    if (toolCalls.length === 0) {
      if (hasSyntheticToolResult) {
        continue;
      }
      // Empty completion: nothing at all this round — no text, no tool call.
      // Weak models and busy endpoints do this intermittently; it is not a real
      // end of turn, so retry a bounded number of times rather than silently
      // ending (which for an adapter turn means the user got no reply at all).
      if (
        shouldRetryEmptyCompletion(
          events,
          toolCalls,
          emptyRetries,
          MAX_EMPTY_RETRIES,
        )
      ) {
        emptyRetries += 1;
        round -= 1; // a retry must not consume the tool-round-trip budget
        continue;
      }
      return latestEventId;
    }
    emptyRetries = 0; // a productive round clears the empty-retry counter

    let sends = 0;
    let sendsSkipped = 0;
    for (const toolCall of toolCalls) {
      const isSend = toolCall.request.functionName === "send_adapter_message";
      if (isSend) {
        sends += 1;
        if (repliedSinceWork) {
          // Already replied this turn with no work since — skip the duplicate
          // instead of blasting the user with another variant of the reply.
          sendsSkipped += 1;
          continue;
        }
      }
      const toolResultEvents = await runtime.traceToolCall(
        turnParent,
        context,
        toolCall,
        round,
        (toolCall) => tools.executePending([toolCall]),
      );
      if (toolResultEvents.length > 0) {
        latestEventId = await appendTurnEvents(context, toolResultEvents);
      }
      // A send marks "already replied"; any real tool resets it, so a genuine
      // acknowledge → do work → report-result sequence stays allowed.
      repliedSinceWork = isSend;
    }
    // Every send this round was a duplicate — the model is just repeating its
    // reply, so end the turn instead of looping into further repeats.
    if (sends > 0 && sends === sendsSkipped) {
      return latestEventId;
    }
  }
}

async function appendTurnEvents(
  context: TurnContext,
  data: EventData[],
): Promise<string> {
  return (await context.exoharness.current.turn.addEvents(data)).latestEventId;
}

// An empty completion is a round that produced nothing at all — no text, no
// tool call. Weak models and busy endpoints do this intermittently; it is not a
// real end of turn, so the loop retries a bounded number of times.
export function shouldRetryEmptyCompletion(
  events: readonly EventData[],
  toolCalls: readonly unknown[],
  emptyRetries: number,
  maxEmptyRetries: number,
): boolean {
  return (
    toolCalls.length === 0 &&
    events.length === 0 &&
    emptyRetries < maxEmptyRetries
  );
}
