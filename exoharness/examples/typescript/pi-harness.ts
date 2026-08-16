import {
  appendCustomEvent,
  assistantTextMessage,
  defineHarness,
  messageText,
  messagesEvent,
  messagesToTranscript,
  toolRequestedEvent,
  toolResultEvent,
  turnMetadata,
  type EventData,
  type JsonObject,
  type JsonValue,
  type Message,
  type PendingToolCall,
  type TurnContext,
} from "@exo/harness";
import {
  traceExecutorTurn,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";
import {
  appendAndTraceObservedToolEvents,
  materializePriorConversationMessages,
  resolveLlmBinding,
  sandboxCwd,
  WarmJsonlSandboxWorker,
  WarmResourceCache,
  type ResolvedLlmBinding,
} from "@exo/model-runtime/shared";
import {
  type PiWorkerEvent,
  type PiWorkerRequest,
  type PiWorkerRunResult,
} from "@exo/pi/protocol";

interface PiTraceState {
  finalText: string;
  promptMessages: Message[];
  rawMessages: JsonValue[];
  runResult: PiWorkerRunResult | null;
  sawTextDelta: boolean;
  startedAt: number;
  streamedText: string;
  ttftMs: number | null;
  observedToolCalls: Map<string, PendingToolCall>;
}

type PiSandboxWorker = WarmJsonlSandboxWorker<PiWorkerRequest, PiWorkerEvent>;

const piWorkers = new WarmResourceCache<PiSandboxWorker>();

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    await traceExecutorTurn(context, (turnParent) =>
      runPiHarnessTurn(context, turnParent, modelBinding),
    );
  },
});

async function runPiHarnessTurn(
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): Promise<string | null> {
  const state: PiTraceState = {
    finalText: "",
    promptMessages: await materializePriorConversationMessages(context),
    rawMessages: [],
    runResult: null,
    sawTextDelta: false,
    startedAt: Date.now(),
    streamedText: "",
    ttftMs: null,
    observedToolCalls: new Map(),
  };
  const prompt = piPrompt(context, state.promptMessages);

  await appendCustomEvent(context.exoharness.current.turn, "pi_turn_started", {
    metadata: turnMetadata(context),
    model: modelBinding.model,
    cwd: sandboxCwd(context),
    hydrated_from: "exoharness_events",
    sandbox_command: piSandboxCommand(context).join(" "),
  });

  try {
    const result = await tracePiRun(
      turnParent,
      context,
      state,
      prompt,
      modelBinding,
    );
    state.runResult = result;
    state.finalText = result.finalText || state.finalText;
    await streamFinalTextSuffix(context, state);
    await appendPiFinalEvents(context, state, result);
    if (result.status === "error") {
      throw new Error(result.error || "Pi run failed");
    }
  } finally {
    await flushPiRawMessages(context, state);
  }
  return null;
}

async function tracePiRun(
  turnParent: TraceParent,
  context: TurnContext,
  state: PiTraceState,
  prompt: string,
  modelBinding: ResolvedLlmBinding,
): Promise<PiWorkerRunResult> {
  return tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        const result = await runPiSandboxWorker(
          context,
          turnParent,
          state,
          prompt,
          modelBinding,
        );
        span.log({
          input: [piPromptMessage(prompt)],
          output: piTraceOutput(state, result),
          metrics: piTraceMetrics(state, result),
        });
        return result;
      } catch (error) {
        const message = errorMessage(error);
        span.log({
          input: [piPromptMessage(prompt)],
          output: piTraceOutput(state, state.runResult),
          metrics: piTraceMetrics(state, state.runResult),
          error: message,
        });
        await appendCustomEvent(
          context.exoharness.current.turn,
          "pi_run_failed",
          { metadata: turnMetadata(context), error: message },
        );
        throw error;
      }
    },
    {
      name: `pi:${modelBinding.model}`,
      type: "llm",
      spanAttributes: { purpose: "pi_turn" },
      event: {
        input: [piPromptMessage(prompt)],
        metadata: {
          ...turnMetadata(context),
          runtime: "pi_sdk",
          model: modelBinding.model,
          streamed: context.streaming,
        },
      },
    },
  );
}

async function runPiSandboxWorker(
  context: TurnContext,
  turnParent: TraceParent,
  state: PiTraceState,
  prompt: string,
  modelBinding: ResolvedLlmBinding,
): Promise<PiWorkerRunResult> {
  const workerKey = piWarmWorkerKey(context, modelBinding);
  const { resource: worker, reused } = await piWorkers.get(workerKey, () =>
    startPiSandboxWorker(context, modelBinding),
  );
  await appendCustomEvent(context.exoharness.current.turn, "pi_worker_ready", {
    metadata: turnMetadata(context),
    warm_worker_reused: reused,
  });
  const request: PiWorkerRequest = {
    prompt,
    systemPrompt: piSystemPrompt(context),
    model: modelBinding.model,
    baseUrl: modelBinding.baseUrl ?? undefined,
    cwd: sandboxCwd(context),
  };
  try {
    return await worker.request(request, async (event) => {
      await handlePiWorkerEvent(context, turnParent, state, event);
      return event.type === "completed" ? event.result : undefined;
    });
  } catch (error) {
    await piWorkers.delete(workerKey, (cachedWorker) => cachedWorker.close());
    throw error;
  }
}

async function startPiSandboxWorker(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): Promise<PiSandboxWorker> {
  return new WarmJsonlSandboxWorker({
    name: "Pi sandbox worker",
    parseEvent: parseWorkerEvent,
    process: await context.startSandboxProcess({
      command: piSandboxCommand(context),
      env: piSandboxEnv(modelBinding),
    }),
  });
}

async function handlePiWorkerEvent(
  context: TurnContext,
  turnParent: TraceParent,
  state: PiTraceState,
  event: PiWorkerEvent,
): Promise<void> {
  switch (event.type) {
    case "delta":
      await streamTextDelta(context, state, event.text);
      return;
    case "message":
      state.rawMessages.push(event.message);
      state.finalText =
        assistantTextFromRawMessage(event.message) || state.finalText;
      return;
    case "tool_start":
    case "tool_end":
      await handlePiToolEvent(context, turnParent, state, event);
      return;
    case "retry":
      await appendCustomEvent(
        context.exoharness.current.turn,
        event.phase === "start" ? "pi_retry_started" : "pi_retry_finished",
        { metadata: turnMetadata(context), details: event.details },
      );
      return;
    case "compaction":
      await appendCustomEvent(
        context.exoharness.current.turn,
        event.phase === "start"
          ? "pi_compaction_started"
          : "pi_compaction_finished",
        { metadata: turnMetadata(context), details: event.details },
      );
      return;
    case "completed":
      state.runResult = event.result;
      return;
    case "error":
      await appendCustomEvent(
        context.exoharness.current.turn,
        "pi_worker_error",
        {
          metadata: turnMetadata(context),
          error: event.message,
          details: event.error,
        },
      );
      throw new Error(event.message);
  }
}

async function handlePiToolEvent(
  context: TurnContext,
  turnParent: TraceParent,
  state: PiTraceState,
  event: Extract<PiWorkerEvent, { type: "tool_start" | "tool_end" }>,
): Promise<void> {
  const events: EventData[] =
    event.type === "tool_start"
      ? [
          toolRequestedEvent({
            toolCallId: event.callId,
            request: {
              functionName: `pi.${event.name}`,
              arguments: jsonObjectOrEmpty(event.args),
            },
          }),
        ]
      : [
          toolResultEvent(event.callId, {
            result: event.result,
            is_error: event.isError,
          }),
        ];
  await appendAndTraceObservedToolEvents(
    context,
    turnParent,
    events,
    state.observedToolCalls,
    "pi_observed_tool",
  );
}

async function streamTextDelta(
  context: TurnContext,
  state: PiTraceState,
  text: string,
): Promise<void> {
  if (!text) {
    return;
  }
  if (!state.sawTextDelta) {
    state.sawTextDelta = true;
    state.ttftMs = Date.now() - state.startedAt;
    if (context.streaming) {
      await context.stream.firstChunk(state.ttftMs);
    }
  }
  state.streamedText += text;
  if (context.streaming) {
    await context.stream.text(text);
  }
}

async function streamFinalTextSuffix(
  context: TurnContext,
  state: PiTraceState,
): Promise<void> {
  if (!state.finalText || !context.streaming) {
    return;
  }
  if (!state.sawTextDelta) {
    await streamTextDelta(context, state, state.finalText);
    return;
  }
  if (state.finalText.startsWith(state.streamedText)) {
    const suffix = state.finalText.slice(state.streamedText.length);
    if (suffix) {
      await streamTextDelta(context, state, suffix);
    }
  }
}

async function appendPiFinalEvents(
  context: TurnContext,
  state: PiTraceState,
  result: PiWorkerRunResult,
): Promise<void> {
  if (state.finalText) {
    await context.exoharness.current.turn.addEvents([
      messagesEvent([assistantTextMessage(state.finalText)]),
    ]);
  }
  await appendCustomEvent(context.exoharness.current.turn, "pi_run_completed", {
    metadata: turnMetadata(context),
    status: result.status,
    model: result.model,
    provider: result.provider,
    usage: result.usage,
  });
}

async function flushPiRawMessages(
  context: TurnContext,
  state: PiTraceState,
): Promise<void> {
  if (state.rawMessages.length === 0) {
    return;
  }
  await appendCustomEvent(context.exoharness.current.turn, "pi_messages", {
    metadata: turnMetadata(context),
    messages: state.rawMessages,
  });
  state.rawMessages = [];
}

function piPrompt(context: TurnContext, priorMessages: Message[]): string {
  const transcript = messagesToTranscript(priorMessages);
  const currentInput = context.request.input.map(messageText).join("\n\n");
  return [
    transcript ? `Conversation so far:\n\n${transcript}` : null,
    `Current user input:\n\n${currentInput}`,
  ]
    .filter(Boolean)
    .join("\n\n");
}

function piSystemPrompt(context: TurnContext): string {
  const instructions = context.agentConfig.instructions
    .map(messageText)
    .filter(Boolean)
    .join("\n\n");
  return [
    "You are Pi running inside an Exoharness-managed sandbox.",
    "Exoharness is the source of truth for durable conversation history. Treat the transcript in each prompt as canonical prior state.",
    "Only files exposed inside the sandbox are available. Do not claim access to host paths that are not mounted.",
    `Your working directory is ${sandboxCwd(context)}.`,
    instructions ? `Agent instructions:\n\n${instructions}` : null,
  ]
    .filter(Boolean)
    .join("\n\n");
}

function piPromptMessage(prompt: string): Message {
  return { role: "user", content: prompt };
}

function assistantTextFromRawMessage(message: JsonValue): string {
  if (!isRecord(message) || message.role !== "assistant") {
    return "";
  }
  const content = Array.isArray(message.content) ? message.content : [];
  return content
    .map((part) => {
      return isRecord(part) &&
        part.type === "text" &&
        typeof part.text === "string"
        ? part.text
        : "";
    })
    .join("");
}

function piTraceOutput(
  state: PiTraceState,
  result: PiWorkerRunResult | null,
): Record<string, unknown> {
  const finalText = result?.finalText || state.finalText;
  return {
    messages: finalText ? [assistantTextMessage(finalText)] : [],
    status: result?.status ?? "unknown",
  };
}

function piTraceMetrics(
  state: PiTraceState,
  result: PiWorkerRunResult | null,
): Record<string, number> {
  const metrics: Record<string, number> = {};
  if (result) {
    metrics.prompt_tokens = result.usage.input;
    metrics.completion_tokens = result.usage.output;
    metrics.tokens = result.usage.totalTokens;
    metrics.prompt_cached_tokens = result.usage.cacheRead;
    metrics.prompt_cache_creation_tokens = result.usage.cacheWrite;
    metrics.estimated_cost = result.usage.cost;
  }
  if (state.ttftMs !== null) {
    metrics.time_to_first_token = state.ttftMs / 1000;
  }
  return metrics;
}

function piSandboxCommand(context: TurnContext): string[] {
  const shell = context.conversationConfig.shellProgram ?? "/bin/bash";
  const command =
    process.env.EXO_PI_SANDBOX_COMMAND ??
    "cd /opt/exo && node --import tsx exoharness/typescript/pi/sandbox-worker.ts";
  return [shell, "-lc", command];
}

function piSandboxEnv(
  modelBinding: ResolvedLlmBinding,
): Record<string, string> {
  const env: Record<string, string> = {
    HOME: process.env.EXO_PI_HOME ?? "/home/exo",
    PI_OFFLINE: "1",
  };
  if (modelBinding.apiKey) {
    env.EXO_PI_API_KEY = modelBinding.apiKey;
  }
  return env;
}

function piWarmWorkerKey(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): string {
  return JSON.stringify({
    agent_id: context.exoharness.current.agent.record.id,
    conversation_id: context.exoharness.current.conversation.record.id,
    model_binding: modelBinding.name,
    model: modelBinding.model,
    base_url: modelBinding.baseUrl ?? null,
    cwd: sandboxCwd(context),
    command: piSandboxCommand(context),
  });
}

function parseWorkerEvent(line: string): PiWorkerEvent {
  const parsed = JSON.parse(line) as unknown;
  if (!isRecord(parsed) || typeof parsed.type !== "string") {
    throw new Error(`invalid Pi sandbox worker event: ${line}`);
  }
  return parsed as PiWorkerEvent;
}

function jsonObjectOrEmpty(value: JsonValue): JsonObject {
  return isRecord(value) ? value : {};
}

function isRecord(value: unknown): value is Record<string, JsonValue> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
