import {
  createToolRegistry,
  messagesEvent,
  registerAgentToolsFromDirectoryIfExists,
  registerBuiltInTools,
  registerLibraryToolModulePath,
  turnMetadata,
  userTextMessage,
  type BuiltInToolName,
  type EventData,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";
import {
  isOpenRouterBinding,
  modelRequiresResponsesApi,
  responseToLinguaEvents,
  responseToolCalls,
  runtimeFromModelBinding,
  type NativeResponsesRequest,
  type ResponsesRuntimeLike,
  type TraceParent,
} from "@exo/model-runtime/responses";
import { ensureTable } from "@exo/model-runtime/cost";

import {
  compressMessagesIfNeeded,
  DEFAULT_MAX_OUTPUT_TOKENS,
  stripVisionImageParts,
} from "./context-compress.js";
import {
  isContextWindowError,
  learnContextWindowTokens,
  parseContextWindowFromError,
} from "./context-window.js";
import {
  materializeExoWorkerPromptMessages,
  splitAssistantToolCallsForResponses,
} from "./message-materialize.js";
import {
  buildTextOnlyNudgeMessage,
  extractAssistantTextFromEvents,
  isRoundBudgetExhausted,
  resolveMaxTextOnlyNudges,
  shouldExitOnTextOnly,
} from "./turn-loop-nudge.js";
import { resolveLlmBinding } from "../typescript/shared.js";
import {
  buildRoundBudgetContinueMessage,
  DEFAULT_ROUND_BUDGET_EXTENSIONS,
  isTaskTreeFinished,
  readTaskTreeSnapshot,
} from "./tools/task-tree-snapshot.js";
import {
  extractProviderUsage,
  promptUsageEvent,
  providerUsageEvent,
  type ProviderUsage,
} from "./provider-usage.js";

export {
  extractProviderUsage,
  PROMPT_USAGE_EVENT_TYPE,
  PROVIDER_USAGE_EVENT_TYPE,
  promptUsageEvent,
  providerUsageEvent,
} from "./provider-usage.js";
export type { ProviderUsage } from "./provider-usage.js";

export interface ExoWorkerTurnLoopOptions {
  instructions?: (context: TurnContext) => Message[] | Promise<Message[]>;
  registerTools?: (
    tools: HarnessToolRegistry,
    context: TurnContext,
  ) => Promise<void> | void;
}

export async function runExoWorkerHarnessTurn(
  context: TurnContext,
  options: ExoWorkerTurnLoopOptions = {},
): Promise<void> {
  await ensureTable();
  const modelBinding = await resolveLlmBinding(context);
  const runtime = runtimeFromModelBinding(context.agentConfig, modelBinding);
  const usesResponsesApi =
    modelRequiresResponsesApi(modelBinding.model) &&
    !isOpenRouterBinding(modelBinding);
  await runtime.runTurn(context, (turnParent) =>
    runExoWorkerTurnLoop(
      runtime,
      context,
      turnParent,
      modelBinding.model,
      usesResponsesApi,
      options,
    ),
  );
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
    content: [
      "install_agent_tool is available this turn. Use it when you need a named, reusable helper that you will call again (or that clarifies a multi-step workflow).",
      "Good triggers: wrapping an HTTP API with fetch, parsing/validating a recurring format, packaging a multi-command workflow into one tool, or bridging two platform tools with custom glue.",
      "Prefer an existing registered host tool when it already does the job. Prefer shell (or other command tools) for a true one-shot. Prefer install_agent_tool when the same logic would otherwise be copy-pasted across rounds.",
      "Call install_agent_tool with a complete TypeScript moduleSource. Do not claim success unless it returns ok: true. The new tool is available in the next model round of the same turn.",
      "moduleSource rules: type-only imports from @exo/harness/tool; default-export { definition, initializationParameters, initialize(...) } satisfies Tool; definition.parameters must be a strict JSON schema object with additionalProperties: false; handlers implement execute(args, execution) (not invoke/call); no zod, no external npm packages, no runtime imports from @exo/harness/tool.",
      "Use uninstall_agent_tool to remove obsolete or conflicting agent-created tools.",
    ].join(" "),
  };
}

async function runExoWorkerTurnLoop(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  turnParent: TraceParent,
  model: string,
  usesResponsesApi: boolean,
  options: ExoWorkerTurnLoopOptions,
): Promise<string | null> {
  const { conversation } = context.exoharness.current;
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  const maxTextOnlyNudges = resolveMaxTextOnlyNudges();
  let latestEventId: string | null = null;
  let budgetExtensions = 0;
  let completeTaskCalled = false;
  let textOnlyNudgesUsed = 0;
  /** Exact provider prompt tokens from the previous successful model round. */
  let lastProviderPromptTokens: number | null = null;

  for (let round = 0; ; round += 1) {
    if (
      isRoundBudgetExhausted(
        round,
        maxToolRoundTrips,
        maxTextOnlyNudges,
        completeTaskCalled,
      )
    ) {
      const snapshot = await readTaskTreeSnapshot(context);
      if (isTaskTreeFinished(snapshot)) {
        return latestEventId;
      }
      if (budgetExtensions >= DEFAULT_ROUND_BUDGET_EXTENSIONS) {
        console.warn(
          `[exo-worker] round budget exhausted before complete_task (round=${round}, maxToolRoundTrips=${maxToolRoundTrips ?? "none"}, nudgesUsed=${textOnlyNudgesUsed})`,
        );
        return latestEventId;
      }
      budgetExtensions += 1;
      round = 0;
      latestEventId = await appendTurnEvents(context, [
        messagesEvent([
          userTextMessage(
            buildRoundBudgetContinueMessage(
              budgetExtensions,
              DEFAULT_ROUND_BUDGET_EXTENSIONS,
            ),
          ),
        ]),
      ]);
      continue;
    }

    const tools = createToolRegistry(context);
    if (options.registerTools) {
      await options.registerTools(tools, context);
    } else {
      registerBuiltInTools(tools, context, defaultBuiltInToolNames(context));
      for (const modulePath of context.agentConfig.typescript
        ?.toolModulePaths ?? []) {
        await registerLibraryToolModulePath(tools, context, modulePath);
      }
      if (context.agentConfig.enableAgentToolCreation) {
        await registerAgentToolsFromDirectoryIfExists(
          tools,
          context,
          process.env.EXO_AGENT_TOOLS_DIR?.trim() || undefined,
        );
      }
    }

    let messages = await materializeExoWorkerPromptMessages(
      conversation,
      options.instructions
        ? await options.instructions(context)
        : basicHarnessInstructions(context),
    );
    if (usesResponsesApi) {
      messages = splitAssistantToolCallsForResponses(messages);
    }

    const maxOutputTokens =
      context.agentConfig.maxOutputTokens ?? DEFAULT_MAX_OUTPUT_TOKENS;
    const summarize = async (prompt: string) => {
      const { text, usage } = await summarizeViaRuntime(
        runtime,
        model,
        prompt,
        turnParent,
        round,
      );
      if (usage) {
        latestEventId = await appendTurnEvents(context, [
          providerUsageEvent(usage, model, "compression"),
        ]);
      }
      return text;
    };
    const persistMarker = async (marker: Message) => {
      latestEventId = await appendTurnEvents(context, [
        messagesEvent([marker]),
      ]);
    };

    const compressed = await compressMessagesIfNeeded(messages, {
      model,
      maxOutputTokens,
      summarize,
      persistMarker,
      lastProviderPromptTokens,
    });
    messages = compressed.messages;

    const request: NativeResponsesRequest = {
      model,
      messages,
      tools: tools.definitions(),
      maxOutputTokens: context.agentConfig.maxOutputTokens,
      metadata: turnMetadata(context),
    };

    let response;
    try {
      response = await completeModelRound(
        runtime,
        context,
        request,
        turnParent,
        round,
      );
    } catch (err) {
      if (!isContextWindowError(err)) throw err;
      const parsed = parseContextWindowFromError(err);
      if (parsed) learnContextWindowTokens(model, parsed);
      console.warn(
        `[exo-worker] context window exceeded — forcing compression and retrying` +
          (parsed ? ` (learned limit=${parsed})` : ""),
      );
      const forced = await compressMessagesIfNeeded(messages, {
        model,
        maxOutputTokens,
        summarize,
        persistMarker,
        lastProviderPromptTokens,
        force: true,
        budgets: parsed
          ? {
              contextWindowTokens: parsed,
              usableInputTokens: Math.max(
                1_000,
                parsed - maxOutputTokens - 4_000,
              ),
              thresholdTokens: 0,
              targetTokens: Math.floor(
                Math.max(1_000, parsed - maxOutputTokens - 4_000) * 0.25,
              ),
            }
          : { thresholdTokens: 0 },
      });
      // Always strip vision before deciding to give up. Short histories and
      // summarizer failures return compressed:false, but a few large PNGs in
      // kept recent turns are often the actual overflow.
      const visionStripped = stripVisionImageParts(forced.messages);
      if (visionStripped.strippedCount > 0) {
        console.warn(
          `[exo-worker] stripped ${visionStripped.strippedCount} vision image(s) before context-window retry`,
        );
      }
      if (!forced.compressed && visionStripped.strippedCount === 0) {
        throw err;
      }
      messages = visionStripped.messages;
      response = await completeModelRound(
        runtime,
        context,
        { ...request, messages },
        turnParent,
        round,
      );
    }

    const providerUsage = extractProviderUsage(response);
    if (providerUsage) {
      lastProviderPromptTokens = providerUsage.promptTokens;
      latestEventId = await appendTurnEvents(context, [
        promptUsageEvent(providerUsage, model),
      ]);
    }

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
      const snapshot = await readTaskTreeSnapshot(context);
      if (isTaskTreeFinished(snapshot)) {
        return latestEventId;
      }
      if (
        shouldExitOnTextOnly(
          completeTaskCalled,
          textOnlyNudgesUsed,
          maxTextOnlyNudges,
        )
      ) {
        return latestEventId;
      }

      textOnlyNudgesUsed += 1;
      const lastAssistantText = extractAssistantTextFromEvents(events);
      const nudge = buildTextOnlyNudgeMessage(
        textOnlyNudgesUsed,
        lastAssistantText,
      );
      console.warn(
        `[exo-worker] text-only exit before complete_task — nudge ${textOnlyNudgesUsed}/${maxTextOnlyNudges}`,
      );
      latestEventId = await appendTurnEvents(context, [
        messagesEvent([
          {
            role: "developer",
            content: nudge,
          },
        ]),
      ]);
      continue;
    }

    for (const toolCall of toolCalls) {
      if (toolCall.request.functionName === "complete_task") {
        completeTaskCalled = true;
      }
      const toolResultEvents = await runtime.traceToolCall(
        turnParent,
        context,
        toolCall,
        round,
        (pending) => tools.executePending([pending]),
      );
      if (toolResultEvents.length > 0) {
        latestEventId = await appendTurnEvents(context, toolResultEvents);
      }
    }

    if (isTaskTreeFinished(await readTaskTreeSnapshot(context))) {
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

async function completeModelRound(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  request: NativeResponsesRequest,
  turnParent: TraceParent,
  round: number,
) {
  return context.streaming
    ? runtime.completeStream(
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
    : runtime.complete(request, {
        parent: turnParent,
        roundIndex: round,
      });
}

async function summarizeViaRuntime(
  runtime: ResponsesRuntimeLike,
  model: string,
  prompt: string,
  turnParent: TraceParent,
  round: number,
): Promise<{ text: string; usage: ProviderUsage | null }> {
  const response = await runtime.complete(
    {
      model,
      messages: [{ role: "user", content: prompt }],
      maxOutputTokens: 8_000,
    },
    {
      parent: turnParent,
      roundIndex: round,
    },
  );
  return {
    text: extractAssistantTextFromEvents(responseToLinguaEvents(response)),
    usage: extractProviderUsage(response),
  };
}
