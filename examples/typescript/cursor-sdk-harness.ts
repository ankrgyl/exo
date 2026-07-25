import {
  appendCustomEvent,
  assistantTextMessage,
  defineHarness,
  toolRequestedEvent,
  toolResultEvent,
  messageText,
  messagesEvent,
  messagesToTranscript,
  stringifyValue,
  turnMetadata,
  type EventData,
  type JsonObject,
  type JsonValue,
  type Message,
  type PendingToolCall,
  type TurnContext,
  toJsonValue,
} from "@exo/harness";
import {
  traceExecutorTurn,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";
import {
  toCursorJson,
  cursorModelId,
  type CursorWorkerEvent,
  type CursorWorkerRequest,
  type CursorWorkerRunResult,
} from "@exo/cursor/protocol";
import {
  appendAndTraceObservedToolEvents,
  apiKeyFromModelBinding,
  materializePriorConversationMessages,
  resolveLlmBinding,
  sandboxCwd,
  WarmJsonlSandboxWorker,
  WarmResourceCache,
  type ResolvedLlmBinding,
} from "./shared";

interface CursorTraceState {
  finalText: string;
  llmPromptMessages: Message[];
  rawMessages: JsonValue[];
  startedAt: number;
  streamedText: string;
  ttftMs: number | null;
  sawTextDelta: boolean;
  promptMessages: Message[];
  runResult: CursorWorkerRunResult | null;
  observedToolCalls: Map<string, PendingToolCall>;
}

type CursorSandboxWorker = WarmJsonlSandboxWorker<
  CursorWorkerRequest,
  CursorWorkerEvent
>;

const cursorWorkers = new WarmResourceCache<CursorSandboxWorker>();

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    await traceExecutorTurn(context, (turnParent) =>
      runCursorSdkHarnessTurn(context, turnParent, modelBinding),
    );
  },
});

async function runCursorSdkHarnessTurn(
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): Promise<string | null> {
  const state: CursorTraceState = {
    finalText: "",
    llmPromptMessages: [],
    rawMessages: [],
    startedAt: Date.now(),
    streamedText: "",
    ttftMs: null,
    sawTextDelta: false,
    promptMessages: await materializeCursorPromptMessages(context),
    runResult: null,
    observedToolCalls: new Map(),
  };
  const prompt = cursorPrompt(context, state.promptMessages);
  state.llmPromptMessages = [cursorSdkPromptMessage(prompt)];

  await appendCustomEvent(
    context.exoharness.current.turn,
    "cursor_sdk_turn_started",
    {
      metadata: turnMetadata(context),
      model: modelBinding.model,
      cwd: sandboxCwd(context),
      hydrated_from: "exoharness_events",
      sandbox_command: cursorSandboxCommand(context).join(" "),
    },
  );

  const result = await traceCursorSdkRun(
    turnParent,
    context,
    state,
    prompt,
    modelBinding,
  );
  state.runResult = result;
  state.finalText = finalCursorText(state, result);
  await streamFinalTextSuffix(context, state);
  await appendCursorFinalEvents(context, state, result);
  if (result.status === "error") {
    throw new Error(result.result ?? "Cursor SDK run failed");
  }
  return null;
}

async function traceCursorSdkRun(
  turnParent: TraceParent,
  context: TurnContext,
  state: CursorTraceState,
  prompt: string,
  modelBinding: ResolvedLlmBinding,
) {
  return tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        const result = await runCursorSandboxWorker(
          context,
          turnParent,
          state,
          prompt,
          modelBinding,
        );
        state.runResult = result;
        state.finalText = finalCursorText(state, result);
        span.log({
          input: state.llmPromptMessages,
          output: cursorTraceOutput(state, result),
          metrics: cursorTraceMetrics(state, result),
        });
        return result;
      } catch (error) {
        const message = cursorHarnessErrorMessage(error);
        span.log({
          input: state.llmPromptMessages,
          output: cursorTraceOutput(state, state.runResult),
          metrics: cursorTraceMetrics(state, state.runResult),
          error: message,
        });
        await appendCustomEvent(
          context.exoharness.current.turn,
          "cursor_sdk_run_failed",
          {
            metadata: turnMetadata(context),
            error: message,
          },
        );
        throw error;
      }
    },
    {
      name: `cursor-sdk:${modelBinding.model}`,
      type: "llm",
      spanAttributes: { purpose: "cursor_sdk_turn" },
      event: {
        input: state.llmPromptMessages,
        metadata: {
          ...turnMetadata(context),
          model: modelBinding.model,
          streamed: context.streaming,
        },
      },
    },
  );
}

async function handleCursorDelta(
  context: TurnContext,
  state: CursorTraceState,
  update: unknown,
): Promise<void> {
  const text = textDeltaFromUpdate(update);
  if (!text) {
    return;
  }
  await streamTextDelta(context, state, text);
}

async function runCursorSandboxWorker(
  context: TurnContext,
  turnParent: TraceParent,
  state: CursorTraceState,
  prompt: string,
  modelBinding: ResolvedLlmBinding,
): Promise<CursorWorkerRunResult> {
  const workerKey = cursorWarmWorkerKey(context, modelBinding);
  const { resource: worker, reused } = await cursorWorkers.get(workerKey, () =>
    startCursorSandboxWorker(context, modelBinding),
  );
  await appendCustomEvent(
    context.exoharness.current.turn,
    "cursor_sdk_worker_ready",
    {
      metadata: turnMetadata(context),
      warm_worker_reused: reused,
    },
  );
  const request: CursorWorkerRequest = {
    prompt,
    model: cursorModelId(modelBinding.model),
    cwd: sandboxCwd(context),
    name: `exo:${context.exoharness.current.conversation.record.slug}`,
  };
  try {
    return await worker.request(request, async (event) => {
      await handleCursorWorkerEvent(context, turnParent, state, event);
      return event.type === "completed" ? event.result : undefined;
    });
  } catch (error) {
    await cursorWorkers.delete(workerKey, (cachedWorker) =>
      cachedWorker.close(),
    );
    throw error;
  }
}

async function startCursorSandboxWorker(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): Promise<CursorSandboxWorker> {
  return new WarmJsonlSandboxWorker({
    name: "cursor sandbox worker",
    parseEvent: parseWorkerEvent,
    process: await context.startSandboxProcess({
      command: cursorSandboxCommand(context),
      env: cursorSandboxEnv(modelBinding),
    }),
  });
}

function cursorHarnessErrorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

async function handleCursorWorkerEvent(
  context: TurnContext,
  turnParent: TraceParent,
  state: CursorTraceState,
  event: CursorWorkerEvent,
): Promise<void> {
  switch (event.type) {
    case "run_started":
      await appendCustomEvent(
        context.exoharness.current.turn,
        "cursor_sdk_run_started",
        {
          metadata: turnMetadata(context),
          agent_id: event.agentId,
          run_id: event.runId,
        },
      );
      return;
    case "delta":
      await handleCursorDelta(context, state, event.update);
      return;
    case "message":
      await handleCursorMessage(context, turnParent, state, event.message);
      return;
    case "completed":
      return;
    case "error":
      await appendCustomEvent(
        context.exoharness.current.turn,
        "cursor_sdk_worker_error",
        {
          metadata: turnMetadata(context),
          error: event.message,
          details: event.error,
        },
      );
      throw new Error(event.message);
  }
}

async function handleCursorMessage(
  context: TurnContext,
  turnParent: TraceParent,
  state: CursorTraceState,
  message: unknown,
): Promise<void> {
  if (shouldStoreCursorSdkMessage(message)) {
    state.rawMessages.push(toCursorJson(message));
  }
  await appendAndTraceObservedToolEvents(
    context,
    turnParent,
    projectCursorSdkMessageToolEvents(message),
    state.observedToolCalls,
    "cursor_observed_tool",
  );

  const text = assistantMessageText(message);
  if (text) {
    state.finalText = text;
  }
}

function shouldStoreCursorSdkMessage(message: unknown): boolean {
  const record = recordOrEmpty(message);
  if (record.type === "assistant") {
    return false;
  }
  if (record.type === "thinking") {
    return false;
  }
  return Object.keys(record).length > 0;
}

function projectCursorSdkMessageToolEvents(message: unknown): EventData[] {
  const record = recordOrEmpty(message);
  if (
    record.type !== "tool_call" ||
    typeof record.call_id !== "string" ||
    typeof record.name !== "string"
  ) {
    return [];
  }

  if (record.status === "running") {
    return [
      toolRequestedEvent({
        toolCallId: record.call_id,
        request: {
          functionName: `cursor.${record.name}`,
          arguments: jsonObjectOrEmpty(record.args),
        },
      }),
    ];
  }

  if (record.status === "completed" || record.status === "error") {
    return [
      toolResultEvent(record.call_id, toJsonValue(record.result ?? null)),
    ];
  }

  return [];
}

function jsonObjectOrEmpty(value: unknown): JsonObject {
  return isRecord(value) ? (toJsonValue(value) as JsonObject) : {};
}

function recordOrEmpty(value: unknown): Record<string, unknown> {
  return isRecord(value) ? value : {};
}

async function streamTextDelta(
  context: TurnContext,
  state: CursorTraceState,
  text: string,
): Promise<void> {
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
  state: CursorTraceState,
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

async function appendCursorFinalEvents(
  context: TurnContext,
  state: CursorTraceState,
  result: CursorWorkerRunResult,
): Promise<void> {
  const events: EventData[] = [];
  if (state.finalText) {
    events.push(messagesEvent([assistantTextMessage(state.finalText)]));
  }
  if (events.length > 0) {
    await context.exoharness.current.turn.addEvents(events);
  }
  await flushCursorRawMessages(context, state);
  await appendCustomEvent(
    context.exoharness.current.turn,
    "cursor_sdk_run_completed",
    {
      metadata: turnMetadata(context),
      run_id: result.id,
      status: result.status,
      model: result.model ?? null,
      duration_ms: result.durationMs ?? null,
      result: result.result ?? null,
    },
  );
}

async function flushCursorRawMessages(
  context: TurnContext,
  state: CursorTraceState,
): Promise<void> {
  if (state.rawMessages.length === 0) {
    return;
  }
  await appendCustomEvent(
    context.exoharness.current.turn,
    "cursor_sdk_messages",
    {
      metadata: turnMetadata(context),
      messages: state.rawMessages,
    },
  );
}

function finalCursorText(
  state: CursorTraceState,
  result: CursorWorkerRunResult,
): string {
  if (result.result && result.result.trim()) {
    return result.result;
  }
  return state.finalText;
}

function cursorPrompt(context: TurnContext, promptMessages: Message[]): string {
  const transcript = messagesToTranscript(promptMessages);
  const currentInput = context.request.input.map(messageText).join("\n\n");
  const parts = [
    "You are Cursor running inside exo's exoharness sandbox.",
    "Exoharness is the source of truth for durable conversation history. Treat the transcript below as the canonical prior state.",
    "You may inspect and modify files exposed through the sandbox filesystem. The sandbox mount and network policy are controlled by exo.",
    context.conversationConfig.shellProgram
      ? `Command execution, if available to Cursor, runs inside the exoharness sandbox. Exo sandbox cwd: ${sandboxCwd(context)}.`
      : "Shell commands are disabled for this conversation.",
    transcript ? `Conversation so far:\n\n${transcript}` : null,
    `Current user input:\n\n${currentInput}`,
  ];
  return parts.filter(Boolean).join("\n\n");
}

function cursorSdkPromptMessage(prompt: string): Message {
  return {
    role: "user",
    content: prompt,
  };
}

async function materializeCursorPromptMessages(
  context: TurnContext,
): Promise<Message[]> {
  const priorMessages = await materializePriorConversationMessages(context);
  return [...context.agentConfig.instructions, ...priorMessages];
}

function assistantMessageText(message: unknown): string {
  const record = isRecord(message) ? message : null;
  if (!record || record.type !== "assistant") {
    return "";
  }
  const messageRecord = isRecord(record.message) ? record.message : null;
  const content = Array.isArray(messageRecord?.content)
    ? messageRecord.content
    : [];
  return content
    .map((block) => {
      if (!isRecord(block)) {
        return "";
      }
      if (block.type === "text" && typeof block.text === "string") {
        return block.text;
      }
      if (block.type === "tool_use") {
        return `[tool_use ${String(block.name ?? "unknown")}] ${stringifyValue(block.input)}`;
      }
      return "";
    })
    .join("");
}

function textDeltaFromUpdate(update: unknown): string | null {
  const record = isRecord(update) ? update : null;
  if (!record) {
    return null;
  }
  const type = typeof record.type === "string" ? record.type : "";
  if (
    (type === "text_delta" || type === "token_delta") &&
    typeof record.text === "string"
  ) {
    return record.text;
  }
  if (type === "text_delta" && typeof record.delta === "string") {
    return record.delta;
  }
  if (typeof record.textDelta === "string") {
    return record.textDelta;
  }
  if (typeof record.text === "string" && type.includes("delta")) {
    return record.text;
  }
  return null;
}

function cursorTraceOutput(
  state: CursorTraceState,
  result: CursorWorkerRunResult | null,
): Record<string, unknown> {
  return {
    messages: state.finalText ? [assistantTextMessage(state.finalText)] : [],
    status: result?.status ?? "unknown",
    result: result?.result ?? null,
  };
}

function cursorTraceMetrics(
  state: CursorTraceState,
  result: CursorWorkerRunResult | null,
): Record<string, number> {
  const metrics: Record<string, number> = {};
  if (state.ttftMs !== null) {
    metrics.time_to_first_token = state.ttftMs / 1000;
  }
  if (result?.durationMs !== undefined) {
    metrics.duration = result.durationMs / 1000;
  }
  return metrics;
}

function cursorSandboxCommand(context: TurnContext): string[] {
  const shell = context.conversationConfig.shellProgram ?? "/bin/bash";
  const command =
    process.env.EXO_CURSOR_SANDBOX_COMMAND ??
    "cd /opt/exo && node --import tsx typescript/cursor/sandbox-worker.ts";
  return [shell, "-lc", command];
}

function cursorSandboxEnv(
  modelBinding: ResolvedLlmBinding,
): Record<string, string> {
  const env: Record<string, string> = {};
  env.HOME = process.env.EXO_CURSOR_HOME ?? "/home/exo";
  for (const key of [
    "BRAINTRUST_API_KEY",
    "BRAINTRUST_APP_URL",
    "CURSOR_API_KEY",
    "CURSOR_CLI_PATH",
  ]) {
    const value = process.env[key];
    if (value) {
      env[key] = value;
    }
  }
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("CURSOR_") && value) {
      env[key] = value;
    }
  }
  const apiKey = apiKeyFromModelBinding(modelBinding);
  if (apiKey) {
    env.CURSOR_API_KEY = apiKey;
  }
  return env;
}

function cursorWarmWorkerKey(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): string {
  return JSON.stringify({
    agent_id: context.exoharness.current.agent.record.id,
    conversation_id: context.exoharness.current.conversation.record.id,
    model_binding: modelBinding.name,
    model: modelBinding.model,
    cwd: sandboxCwd(context),
    command: cursorSandboxCommand(context),
  });
}

function parseWorkerEvent(line: string): CursorWorkerEvent {
  const parsed = JSON.parse(line) as unknown;
  if (!isRecord(parsed) || typeof parsed.type !== "string") {
    throw new Error(`invalid cursor sandbox worker event: ${line}`);
  }
  return parsed as CursorWorkerEvent;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
