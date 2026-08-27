import {
  CHARS_PER_TOKEN,
  contextCompactedEvent,
  createToolRegistry,
  estimatePromptTokens,
  materializePromptMessages,
  messagesToTranscript,
  selectCompactionCut,
  registerBuiltInTools,
  registerInstalledTools,
  registerLegacyAgentToolsFromDirectoryIfExists,
  registerLibraryToolModulePath,
  turnMetadata,
  type BuiltInToolName,
  type ContextCompactedPayload,
  type EventData,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";
import {
  responseMessages,
  responseToLinguaEvents,
  responseToolCalls,
  runtimeFromModelBinding,
  type NativeResponsesRequest,
  type NativeTraceOptions,
  type ResponsesRuntimeLike,
  type TraceParent,
} from "@exo/model-runtime/responses";
import { ensureTable, getTable, lookup } from "@exo/model-runtime/cost";

import { resolveLlmBinding } from "./shared";

export interface ResponsesTurnLoopOptions {
  instructions?: (context: TurnContext) => Message[] | Promise<Message[]>;
  registerTools?: (
    tools: HarnessToolRegistry,
    context: TurnContext,
  ) => Promise<void> | void;
  maxContextLength?: number | null; // history is summarized once prompt passes 80% of this.
}

export async function runResponsesHarnessTurn(
  context: TurnContext,
  options: ResponsesTurnLoopOptions = {},
): Promise<void> {
  await ensureTable(); // load the price table once so cost is ready when events are built
  const modelBinding = await resolveLlmBinding(context);
  const runtime = runtimeFromModelBinding(context.agentConfig, modelBinding);
  await runtime.runTurn(context, (turnParent) =>
    runResponsesTurnLoop(runtime, context, turnParent, modelBinding.model, {
      ...options,
      maxContextLength:
        options.maxContextLength ?? modelContextLength(modelBinding.model),
    }),
  );
}

// Context window from the LiteLLM price table (max_input_tokens).
function modelContextLength(model: string): number | null {
  const table = getTable();
  if (!table) {
    return null;
  }
  const entry = lookup(table, model) ?? lookup(table, `openrouter/${model}`);
  return entry?.max_input_tokens ?? null;
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

async function runResponsesTurnLoop(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  turnParent: TraceParent,
  model: string,
  options: ResponsesTurnLoopOptions,
): Promise<string | null> {
  const { conversation } = context.exoharness.current;
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  let latestEventId: string | null = null;

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
    const instructions = options.instructions
      ? await options.instructions(context)
      : basicHarnessInstructions(context);
    const maxContextLength = options.maxContextLength ?? null;
    let messages = await materializePromptMessages(conversation, instructions, {
      maxContextLength,
    });
    if (
      maxContextLength !== null &&
      shouldCompactContext(messages, maxContextLength) &&
      (await compactConversationContext(runtime, context, {
        model,
        instructions,
        maxContextLength,
        trace: { parent: turnParent, roundIndex: round },
        metadata: turnMetadata(context),
      }))
    ) {
      messages = await materializePromptMessages(conversation, instructions, {
        maxContextLength,
      });
    }
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
      return latestEventId;
    }

    for (const toolCall of toolCalls) {
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
    }
  }
}

async function appendTurnEvents(
  context: TurnContext,
  data: EventData[],
): Promise<string> {
  return (await context.exoharness.current.turn.addEvents(data)).latestEventId;
}

// Context Compaction will summarize the oldest ~70% of history and append a
// `context_compacted` event.
const COMPACTION_THRESHOLD = 0.8;
const SUMMARY_MAX_OUTPUT_TOKENS = 4096;
// Room left for the summary itself when sizing the tail that stays verbatim.
const SUMMARY_RESERVE_TOKENS = 1000;

function shouldCompactContext(
  messages: Message[],
  maxContextLength: number,
): boolean {
  return (
    estimatePromptTokens(messages) > maxContextLength * COMPACTION_THRESHOLD
  );
}

interface CompactConversationOptions {
  model: string;
  instructions: Message[];
  maxContextLength: number;
  trace?: NativeTraceOptions;
  metadata?: Record<string, string>;
}

// Returns null when nothing was compacted (no valid cut, or the summary call
// failed); the hard trim in materializePromptMessages still bounds the prompt.
async function compactConversationContext(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  options: CompactConversationOptions,
): Promise<ContextCompactedPayload | null> {
  const { conversation, turn } = context.exoharness.current;
  const { events } = await conversation.getEvents({
    direction: "asc",
    types: ["messages", "tool_requested", "tool_result", "context_compacted"],
  });
  // Cut enough that instructions + summary + tail land under the threshold.
  const tailTokens =
    options.maxContextLength * COMPACTION_THRESHOLD -
    estimatePromptTokens(options.instructions) -
    SUMMARY_RESERVE_TOKENS;
  const cut = selectCompactionCut(
    events,
    Math.max(0, tailTokens) * CHARS_PER_TOKEN,
  );
  if (!cut) {
    return null;
  }

  const transcript = messagesToTranscript(cut.headMessages);
  let summary: string;
  try {
    summary = await summarizeTranscript(runtime, transcript, options);
  } catch (error) {
    console.error(
      `context compaction: summary failed, skipping: ${error instanceof Error ? error.message : String(error)}`,
    );
    return null;
  }

  const payload: ContextCompactedPayload = {
    summary,
    covers_through_event_id: cut.coversThroughEventId,
    summarized_messages: cut.headMessages.length,
    model: options.model,
  };
  await turn.addEvents([contextCompactedEvent(payload)]);
  console.error(
    `context compaction: summarized ${payload.summarized_messages} messages through event ${payload.covers_through_event_id}`,
  );
  return payload;
}

const SUMMARIZER_INSTRUCTIONS = `You are compacting the context of a long-running assistant conversation so the assistant can continue with a shorter prompt. Write a dense, factual summary of the transcript you are given. The transcript is the beginning of a longer conversation: more recent messages follow it and stay visible to the assistant verbatim, so do not treat requests near the end as unfulfilled and do not repeat their answers. Preserve, in priority order: the user's goals, standing instructions, and unfulfilled requests; the state of in-progress work and what remains; decisions and their reasons, and commitments made to the user; concrete facts discovered (names, identifiers, paths, URLs, commands, values, errors and outcomes); anything the user said about how the assistant should behave. Drop pleasantries and dead ends unless they inform current work. Do not invent details. Write in the third person ("the user asked…", "the assistant found…"). Output only the summary.`;

async function summarizeTranscript(
  runtime: ResponsesRuntimeLike,
  transcript: string,
  options: CompactConversationOptions,
): Promise<string> {
  const response = await runtime.complete(
    {
      model: options.model,
      messages: [
        { role: "developer", content: SUMMARIZER_INSTRUCTIONS },
        { role: "user", content: `Transcript to summarize:\n\n${transcript}` },
      ],
      tools: [],
      maxOutputTokens: SUMMARY_MAX_OUTPUT_TOKENS,
      metadata: options.metadata,
    },
    options.trace,
  );
  const text = responseText(response).trim();
  if (text.length === 0) {
    throw new Error("summary model returned no text");
  }
  return text;
}

// Text parts of the assistant output only; reasoning items are left out.
function responseText(
  response: Parameters<typeof responseMessages>[0],
): string {
  return responseMessages(response)
    .filter((message) => message.role === "assistant")
    .map((message) =>
      typeof message.content === "string"
        ? message.content
        : Array.isArray(message.content)
          ? message.content
              .map((part) =>
                part &&
                typeof part === "object" &&
                (part as { type?: unknown }).type === "text" &&
                typeof (part as { text?: unknown }).text === "string"
                  ? (part as { text: string }).text
                  : "",
              )
              .join("")
          : "",
    )
    .join("\n");
}
