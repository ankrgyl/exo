import { EventEmitter } from "node:events";
import { PassThrough, Writable } from "node:stream";

import {
  query,
  type Options,
  type Query,
  type SDKMessage,
  type SDKResultMessage,
  type SDKUserMessage,
  type SpawnedProcess,
  type SpawnOptions,
} from "@anthropic-ai/claude-agent-sdk";
import {
  appendCustomEvent,
  assistantTextMessage,
  defineHarness,
  materializeConversationMessages,
  messageText,
  messagesEvent,
  messagesToTranscript,
  systemTextMessage,
  toJsonValue,
  turnMetadata,
  type JsonValue,
  type Message,
  type PendingToolCall,
  type TurnContext,
} from "@exo/harness";
import {
  errorMessage,
  ResponsesRuntime,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";

import {
  appendEvents,
  apiKeyFromModelBinding,
  appendAndTraceObservedToolEvents,
  asRecord,
  markFirstTextDelta,
  pickEnv,
  pickEnvFrom,
  projectAnthropicMessageToolEvents,
  resolveLlmBinding,
  sandboxCwd,
  type ResolvedLlmBinding,
} from "./shared";

const DEFAULT_CLAUDE_CODE_SANDBOX_EXECUTABLE = "/usr/local/bin/claude-code";
const CLAUDE_RESULT_GRACE_MS = 5_000;
const CLAUDE_MAX_API_RETRIES = 2;
const CLAUDE_STDERR_PREVIEW_CHARS = 4_000;
const CLAUDE_STARTUP_TIMEOUT_MS = 20_000;

interface ClaudeTraceState {
  startedAt: number;
  finalText: string;
  systemPrompt: string | null;
  promptMessages: Message[];
  rawMessages: JsonValue[];
  ttftMs: number | null;
  sawTextDelta: boolean;
  result: SDKResultMessage | null;
  finalMessageStored: boolean;
  observedToolCalls: Map<string, PendingToolCall>;
}

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    const runtime = ResponsesRuntime.fromModelBinding(
      context.agentConfig,
      modelBinding,
    );
    await runtime.runTurn(context, (turnParent) =>
      runClaudeCodeTurn(context, turnParent, modelBinding),
    );
  },
});

async function runClaudeCodeTurn(
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): Promise<string | null> {
  const systemPrompt = claudeSystemPrompt(context);
  const state: ClaudeTraceState = {
    startedAt: Date.now(),
    finalText: "",
    systemPrompt,
    promptMessages: await materializeClaudePromptMessages(
      context,
      systemPrompt,
    ),
    rawMessages: [],
    ttftMs: null,
    sawTextDelta: false,
    result: null,
    finalMessageStored: false,
    observedToolCalls: new Map(),
  };

  await appendCustomEvent(
    context.exoharness.current.turn,
    "claude_turn_started",
    {
      metadata: turnMetadata(context),
      model: modelBinding.model,
      hydrated_from: "exoharness_events",
    },
  );

  try {
    await traceClaudeLlmTurn(
      turnParent,
      context,
      state,
      modelBinding,
      async () => {
        await consumeClaudeQuery(
          query({
            prompt: claudePromptInput(claudePrompt(state.promptMessages)),
            options: claudeOptions(context, state.systemPrompt, modelBinding),
          }),
          context,
          turnParent,
          state,
        );
      },
    );

    await appendClaudeFinalMessage(context, state);

    if (state.result?.type === "result" && state.result.is_error) {
      throw new Error(claudeResultError(state.result));
    }
  } finally {
    await flushClaudeRawMessages(context, state);
  }
  return null;
}

async function* claudePromptInput(
  prompt: string,
): AsyncIterable<SDKUserMessage> {
  yield {
    type: "user",
    message: {
      role: "user",
      content: prompt,
    },
    parent_tool_use_id: null,
  };
}

async function consumeClaudeQuery(
  claudeQuery: Query,
  context: TurnContext,
  turnParent: TraceParent,
  state: ClaudeTraceState,
): Promise<void> {
  let graceTimer: ReturnType<typeof setTimeout> | null = null;
  let startupTimedOut = false;
  let sawSdkMessage = false;
  const startupTimer = setTimeout(() => {
    if (!sawSdkMessage) {
      startupTimedOut = true;
      claudeQuery.close();
    }
  }, CLAUDE_STARTUP_TIMEOUT_MS);
  startupTimer.unref?.();

  const clearGraceTimer = () => {
    if (graceTimer) {
      clearTimeout(graceTimer);
      graceTimer = null;
    }
  };

  const scheduleGraceClose = () => {
    if (state.result || graceTimer) {
      return;
    }
    graceTimer = setTimeout(() => {
      claudeQuery.close();
    }, CLAUDE_RESULT_GRACE_MS);
    graceTimer.unref?.();
  };

  try {
    for await (const message of claudeQuery) {
      sawSdkMessage = true;
      clearTimeout(startupTimer);
      await handleClaudeMessage(context, turnParent, state, message);
      const apiRetryError = claudeApiRetryLimitError(message);
      if (apiRetryError) {
        throw new Error(apiRetryError);
      }
      if (message.type === "result") {
        clearGraceTimer();
        claudeQuery.close();
        break;
      }
      if (
        message.type === "assistant" &&
        state.finalText &&
        !claudeAssistantHasToolUse(message.message.content)
      ) {
        scheduleGraceClose();
      }
    }
    if (startupTimedOut && !state.result && !state.finalText) {
      throw new Error(
        `Claude Code produced no SDK messages within ${CLAUDE_STARTUP_TIMEOUT_MS}ms; check claude_process_stderr events for process startup failures.`,
      );
    }
  } catch (error) {
    if (!state.finalText) {
      throw error;
    }
    await appendCustomEvent(
      context.exoharness.current.turn,
      "claude_query_closed_after_text",
      {
        metadata: turnMetadata(context),
        error: errorMessage(error),
        grace_ms: CLAUDE_RESULT_GRACE_MS,
      },
    );
  } finally {
    clearTimeout(startupTimer);
    clearGraceTimer();
    claudeQuery.close();
  }
}

async function traceClaudeLlmTurn(
  turnParent: TraceParent,
  context: TurnContext,
  state: ClaudeTraceState,
  modelBinding: ResolvedLlmBinding,
  run: () => Promise<void>,
): Promise<void> {
  await tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        await run();
        span.log({
          input: state.promptMessages,
          output: claudeTraceOutput(state),
          metrics: claudeUsageMetrics(state),
        });
      } catch (error) {
        span.log({
          input: state.promptMessages,
          output: claudeTraceOutput(state),
          metrics: claudeUsageMetrics(state),
          error: errorMessage(error),
        });
        throw error;
      }
    },
    {
      name: `claude-code:${modelBinding.model}`,
      type: "llm",
      spanAttributes: { purpose: "claude_code_llm_turn" },
      event: {
        input: state.promptMessages,
        metadata: {
          ...turnMetadata(context),
          runtime: "claude_agent_sdk",
          model: modelBinding.model,
          streamed: context.streaming,
        },
      },
    },
  );
}

function claudeOptions(
  context: TurnContext,
  systemPrompt: string | null,
  modelBinding: ResolvedLlmBinding,
): Options {
  const options: Options = {
    model: modelBinding.model,
    cwd: sandboxCwd(context),
    persistSession: false,
    includePartialMessages: true,
    env: claudeSandboxBaseEnv(modelBinding),
    pathToClaudeCodeExecutable: claudeSandboxExecutable(),
    spawnClaudeCodeProcess: (options) =>
      new SandboxClaudeCodeProcess(context, options),
  };
  if (systemPrompt) {
    return { ...options, systemPrompt };
  }
  return options;
}

async function handleClaudeMessage(
  context: TurnContext,
  turnParent: TraceParent,
  state: ClaudeTraceState,
  message: SDKMessage,
): Promise<void> {
  if (shouldStoreClaudeSdkMessage(message)) {
    state.rawMessages.push(toJsonValue(message));
  }

  if (message.type === "stream_event") {
    await handleClaudeStreamEvent(context, state, message.event);
    return;
  }

  if (message.type === "assistant") {
    await appendAndTraceObservedToolEvents(
      context,
      turnParent,
      projectAnthropicMessageToolEvents(message, {
        toolNamePrefix: "claude.",
      }),
      state.observedToolCalls,
      "claude_observed_tool",
    );
    const text = claudeAssistantText(message.message.content);
    if (text) {
      state.finalText = text;
    }
    return;
  }

  if (message.type === "user") {
    await appendAndTraceObservedToolEvents(
      context,
      turnParent,
      projectAnthropicMessageToolEvents(message, {
        toolNamePrefix: "claude.",
      }),
      state.observedToolCalls,
      "claude_observed_tool",
    );
    return;
  }

  if (message.type === "result") {
    state.result = message;
    await appendCustomEvent(context.exoharness.current.turn, "claude_result", {
      metadata: turnMetadata(context),
      result: toJsonValue(message),
    });
  }
}

function shouldStoreClaudeSdkMessage(message: SDKMessage): boolean {
  return message.type !== "stream_event";
}

async function appendClaudeFinalMessage(
  context: TurnContext,
  state: ClaudeTraceState,
): Promise<void> {
  if (!state.finalText || state.finalMessageStored) {
    return;
  }
  state.finalMessageStored = true;
  await appendEvents(context, [
    messagesEvent([assistantTextMessage(state.finalText)]),
  ]);
}

function claudeApiRetryLimitError(message: SDKMessage): string | null {
  const record = asRecord(message);
  if (record.type !== "system" || record.subtype !== "api_retry") {
    return null;
  }
  const attempt = record.attempt;
  if (typeof attempt !== "number" || attempt < CLAUDE_MAX_API_RETRIES) {
    return null;
  }
  const maxRetries =
    typeof record.max_retries === "number" ? record.max_retries : "unknown";
  const error = typeof record.error === "string" ? record.error : "unknown";
  const status =
    typeof record.error_status === "number" ? record.error_status : "none";
  return `Claude Code API request is still retrying after attempt ${attempt}/${maxRetries} (status: ${status}, error: ${error}); aborting instead of waiting for the full SDK retry backoff.`;
}

async function handleClaudeStreamEvent(
  context: TurnContext,
  state: ClaudeTraceState,
  event: unknown,
): Promise<void> {
  const record = asRecord(event);
  if (record.type !== "content_block_delta") {
    return;
  }
  const delta = asRecord(record.delta);
  if (delta.type !== "text_delta" || typeof delta.text !== "string") {
    return;
  }

  const ttftMs = markFirstTextDelta(state);
  if (ttftMs !== null) {
    if (context.streaming) {
      await context.stream.firstChunk(ttftMs);
    }
  }

  if (context.streaming) {
    await context.stream.text(delta.text);
  }
}

async function materializeClaudePromptMessages(
  context: TurnContext,
  systemPrompt: string | null,
): Promise<Message[]> {
  const messages = await materializeConversationMessages(
    context.exoharness.current.conversation,
  );
  const promptMessages = messages.filter(
    (message) => message.role !== "system" && message.role !== "developer",
  );
  if (!systemPrompt) {
    return promptMessages;
  }
  return [systemTextMessage(systemPrompt), ...promptMessages];
}

function claudePrompt(messages: Message[]): string {
  const conversational = messages.filter(
    (message) => message.role !== "system" && message.role !== "developer",
  );
  return messagesToTranscript(conversational);
}

function claudeSystemPrompt(context: TurnContext): string | null {
  const instructions = context.agentConfig.instructions
    .map(messageText)
    .filter(Boolean)
    .join("\n\n");
  return instructions || null;
}

function claudeAssistantText(content: unknown): string {
  if (!Array.isArray(content)) {
    return "";
  }
  return content
    .map((part) => {
      const record = asRecord(part);
      if (record.type === "text" && typeof record.text === "string") {
        return record.text;
      }
      return "";
    })
    .join("");
}

function claudeAssistantHasToolUse(content: unknown): boolean {
  return (
    Array.isArray(content) &&
    content.some((part) => asRecord(part).type === "tool_use")
  );
}

function claudeTraceOutput(state: ClaudeTraceState): Record<string, unknown> {
  return {
    messages: state.finalText ? [assistantTextMessage(state.finalText)] : [],
    tool_calls: [],
    status: state.result?.subtype ?? "completed",
  };
}

function claudeUsageMetrics(state: ClaudeTraceState): Record<string, number> {
  const metrics: Record<string, number> = {};
  const usage = state.result?.usage;
  if (usage) {
    metrics.prompt_tokens = usage.input_tokens;
    metrics.completion_tokens = usage.output_tokens;
    metrics.tokens = usage.input_tokens + usage.output_tokens;
    metrics.prompt_cached_tokens = usage.cache_read_input_tokens ?? 0;
    metrics.prompt_cache_creation_tokens =
      usage.cache_creation_input_tokens ?? 0;
  }
  if (state.result?.total_cost_usd !== undefined) {
    metrics.estimated_cost = state.result.total_cost_usd;
  }
  if (state.ttftMs !== null) {
    metrics.time_to_first_token = state.ttftMs / 1000;
  }
  return metrics;
}

async function flushClaudeRawMessages(
  context: TurnContext,
  state: ClaudeTraceState,
): Promise<void> {
  if (state.rawMessages.length === 0) {
    return;
  }
  await appendCustomEvent(
    context.exoharness.current.turn,
    "claude_sdk_messages",
    {
      metadata: turnMetadata(context),
      messages: state.rawMessages,
    },
  );
}

function claudeResultError(result: SDKResultMessage): string {
  if ("errors" in result && result.errors.length > 0) {
    return result.errors.join("\n");
  }
  const resultText = asRecord(result).result;
  if (typeof resultText === "string" && resultText.trim()) {
    return resultText;
  }
  return result.stop_reason ?? "claude code turn failed";
}

class SandboxClaudeCodeProcess extends EventEmitter implements SpawnedProcess {
  readonly stdin: Writable;
  readonly stdout = new PassThrough();
  killed = false;
  exitCode: number | null = null;
  private readonly pendingWrites: string[] = [];
  private sandboxProcess: Awaited<
    ReturnType<TurnContext["startSandboxProcess"]>
  > | null = null;
  private stdinEnded = false;

  constructor(
    private readonly turnContext: TurnContext,
    options: SpawnOptions,
  ) {
    super();
    this.stdin = new Writable({
      write: (chunk, _encoding, callback) => {
        const data = Buffer.isBuffer(chunk) ? chunk.toString() : String(chunk);
        this.writeStdin(data).then(() => callback(), callback);
      },
      final: (callback) => {
        this.closeStdin().then(() => callback(), callback);
      },
    });

    if (options.signal.aborted) {
      this.kill("SIGTERM");
      return;
    }
    options.signal.addEventListener(
      "abort",
      () => {
        this.kill("SIGTERM");
      },
      { once: true },
    );
    void this.start(this.turnContext, options);
  }

  kill(_signal: NodeJS.Signals): boolean {
    if (this.killed) {
      return true;
    }
    this.killed = true;
    if (this.sandboxProcess) {
      void this.sandboxProcess.close();
    }
    return true;
  }

  private async start(
    context: TurnContext,
    options: SpawnOptions,
  ): Promise<void> {
    try {
      await appendCustomEvent(
        context.exoharness.current.turn,
        "claude_process_starting",
        {
          metadata: turnMetadata(context),
          command: [options.command, ...options.args],
          cwd: options.cwd ?? null,
        },
      );
      const sandboxProcess = await context.startSandboxProcess({
        command: [options.command, ...options.args],
        env: claudeSandboxEnv(options.env),
      });
      await appendCustomEvent(
        context.exoharness.current.turn,
        "claude_process_started",
        {
          metadata: turnMetadata(context),
        },
      );
      if (this.killed) {
        await sandboxProcess.close();
        return;
      }
      void pumpSandboxReadable(sandboxProcess.stdout, this.stdout);
      void drainSandboxStderr(context, sandboxProcess.stderr);
      while (this.pendingWrites.length > 0) {
        const pendingWrite = this.pendingWrites.shift();
        if (pendingWrite !== undefined) {
          await sandboxProcess.writeStdin(pendingWrite);
        }
      }
      this.sandboxProcess = sandboxProcess;
      if (this.stdinEnded) {
        await sandboxProcess.closeStdin();
      }
      const exitCode = await sandboxProcess.wait();
      this.exitCode = exitCode;
      this.emit("exit", exitCode, null);
      this.stdout.end();
    } catch (error) {
      const normalized =
        error instanceof Error ? error : new Error(String(error));
      await appendCustomEvent(
        context.exoharness.current.turn,
        "claude_process_start_failed",
        {
          metadata: turnMetadata(context),
          error: normalized.message,
        },
      );
      this.emit("error", normalized);
      this.stdout.destroy(normalized);
    }
  }

  private async writeStdin(data: string): Promise<void> {
    if (this.killed || this.stdinEnded) {
      return;
    }
    if (!this.sandboxProcess) {
      this.pendingWrites.push(data);
      return;
    }
    await this.sandboxProcess.writeStdin(data);
  }

  private async closeStdin(): Promise<void> {
    if (this.stdinEnded) {
      return;
    }
    this.stdinEnded = true;
    if (!this.sandboxProcess) {
      return;
    }
    await this.sandboxProcess.closeStdin();
  }
}

async function pumpSandboxReadable(
  input: ReadableStream<string>,
  output: PassThrough,
): Promise<void> {
  try {
    const reader = input.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) {
          break;
        }
        output.write(value);
      }
    } finally {
      reader.releaseLock();
    }
  } finally {
    output.end();
  }
}

async function drainSandboxStderr(
  context: TurnContext,
  input: ReadableStream<string>,
): Promise<void> {
  const reader = input.getReader();
  let chunkCount = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        return;
      }
      if (value && chunkCount < 3) {
        chunkCount += 1;
        await appendCustomEvent(
          context.exoharness.current.turn,
          "claude_process_stderr",
          {
            metadata: turnMetadata(context),
            chunk_index: chunkCount,
            text: value.slice(0, CLAUDE_STDERR_PREVIEW_CHARS),
            truncated: value.length > CLAUDE_STDERR_PREVIEW_CHARS,
          },
        );
      }
    }
  } finally {
    reader.releaseLock();
  }
}

function claudeSandboxExecutable(): string {
  return DEFAULT_CLAUDE_CODE_SANDBOX_EXECUTABLE;
}

function claudeSandboxBaseEnv(
  modelBinding: ResolvedLlmBinding,
): Record<string, string> {
  const env = pickEnv((key) => {
    return (
      key.startsWith("CLAUDE_") ||
      key === "BRAINTRUST_API_KEY" ||
      key === "BRAINTRUST_APP_URL" ||
      key === "BRAINTRUST_API_URL"
    );
  });
  const apiKey = apiKeyFromModelBinding(modelBinding);
  if (apiKey) {
    env.ANTHROPIC_API_KEY = apiKey;
  }
  if (modelBinding.baseUrl) {
    env.ANTHROPIC_BASE_URL = modelBinding.baseUrl;
  }
  return env;
}

function claudeSandboxEnv(
  env: Record<string, string | undefined>,
): Record<string, string> {
  const selected = pickEnvFrom(env, (key) => {
    return (
      key.startsWith("ANTHROPIC_") ||
      key.startsWith("CLAUDE_") ||
      key === "TRACEPARENT" ||
      key === "TRACESTATE"
    );
  });
  selected.HOME ??= "/home/exo";
  selected.CLAUDE_CONFIG_DIR ??= "/home/exo/.claude";
  return selected;
}
