import {
  appendCustomEvent,
  assistantTextMessage,
  defineHarness,
  messageText,
  messagesEvent,
  stringifyValue,
  toJsonValue,
  toolRequestedEvent,
  toolResultEvent,
  turnMetadata,
  type EventData,
  type JsonObject,
  type JsonValue,
  type Message,
  type PendingToolCall,
  type SandboxProcess,
  type TurnContext,
} from "@exo/harness";
import {
  CodexAppServer,
  type CodexNotification,
  type CodexProtocolLogEntry,
  type CodexServerRequest,
} from "@exo/codex/app-server";
import { responsesMessagesToLingua } from "@braintrust/lingua";
import {
  errorMessage,
  ResponsesRuntime,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";

import {
  appendEvents,
  asRecord,
  instructionsText,
  isRecord,
  markFirstTextDelta,
  materializePriorConversationMessages,
  numberField,
  objectArgs,
  pickEnv,
  resolveLlmBinding,
  sandboxCwd,
  shellToolResultText,
  shellToolSucceeded,
  stringOrNull,
  traceExoharnessToolCall,
  traceObservedToolCall,
  WarmResourceCache,
  type ResolvedLlmBinding,
} from "./shared";

const CODEX_SHELL_TOOL = "codex.shell";
const CODEX_WEB_SEARCH_TOOL = "codex.web_search";
const EXO_SHELL_TOOL = "shell";
const EXO_SHELL_DYNAMIC_TOOL = "exo_shell";
const CODEX_PRIOR_MESSAGE_MAX_CHARS = 8_000;
const CODEX_PRIOR_TOOL_RESULT_MAX_CHARS = 4_000;
const CODEX_PRIOR_HISTORY_MAX_CHARS = 24_000;
const CODEX_WARM_SESSION_EVENT = "codex_warm_session";

interface CodexTokenUsage {
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  cachedInputTokens?: number;
  reasoningOutputTokens?: number;
}

interface CodexTurnTraceState {
  finalText: string;
  ttftMs: number | null;
  tokenUsage: CodexTokenUsage | null;
  promptMessages: Message[];
  startedAt: number;
  sawTextDelta: boolean;
}

interface CodexWarmTurnScope {
  context: TurnContext;
  protocolLog: CodexProtocolEventBuffer;
  turnParent: TraceParent;
}

interface PriorResponseItems {
  items: JsonValue[];
  sourceMessageCount: number;
  droppedMessageCount: number;
  truncatedMessageCount: number;
  textChars: number;
}

interface PriorResponseItemCandidate {
  item: JsonValue;
  textChars: number;
  truncated: boolean;
}

interface CodexWarmSessionRecord {
  sessionKey: string;
  sandboxId: string | null;
  sandboxProcessId: string | null;
  threadId: string;
}

class CodexWarmSession {
  threadId: string | null;
  private current: CodexWarmTurnScope | null;

  private constructor(
    readonly server: CodexAppServer,
    readonly process: SandboxProcess,
    initialScope: CodexWarmTurnScope,
    threadId: string | null,
  ) {
    this.current = initialScope;
    this.threadId = threadId;
  }

  static async start(
    scope: CodexWarmTurnScope,
    modelBinding: ResolvedLlmBinding,
    sessionKey: string,
  ): Promise<CodexWarmSession> {
    let session: CodexWarmSession | null = null;
    const pendingProtocol: CodexProtocolLogEntry[] = [];
    const process = await scope.context.startSandboxProcess({
      command: codexSandboxCommand(scope.context),
      env: codexSandboxEnv(modelBinding),
      reuseKey: sessionKey,
    });
    const warmRecord = process.reused
      ? await latestCodexWarmSession(scope.context, sessionKey, process)
      : null;
    const options = {
      process,
      onProtocolMessage: (entry: CodexProtocolLogEntry) => {
        if (session) {
          session.recordProtocol(entry);
        } else {
          pendingProtocol.push(entry);
        }
      },
      onServerRequest: (request: CodexServerRequest) =>
        session?.handleServerRequest(request),
    };
    const server = process.reused
      ? await CodexAppServer.attachToSandbox(options)
      : await CodexAppServer.startInSandbox(options);
    session = new CodexWarmSession(
      server,
      process,
      scope,
      process.reused ? (warmRecord?.threadId ?? null) : null,
    );
    for (const entry of pendingProtocol) {
      session.recordProtocol(entry);
    }
    return session;
  }

  setTurnScope(scope: CodexWarmTurnScope): void {
    this.current = scope;
  }

  clearTurnScope(scope: CodexWarmTurnScope): void {
    if (this.current === scope) {
      this.current = null;
    }
  }

  close(): void {
    this.server.close();
  }

  private recordProtocol(entry: CodexProtocolLogEntry): void {
    this.current?.protocolLog.record(entry);
  }

  private handleServerRequest(
    request: CodexServerRequest,
  ): Promise<JsonValue | undefined> | JsonValue | undefined {
    const current = this.current;
    if (!current) {
      return undefined;
    }
    return handleCodexServerRequest(
      current.context,
      current.turnParent,
      request,
    );
  }
}

const codexSessions = new WarmResourceCache<CodexWarmSession>();

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    const runtime = ResponsesRuntime.fromModelBinding(
      context.agentConfig,
      modelBinding,
    );
    await runtime.runTurn(context, (turnParent) =>
      runCodexTurn(context, turnParent, modelBinding),
    );
  },
});

async function runCodexTurn(
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): Promise<string | null> {
  await requireCodexSandboxNetworking(context);

  const { turn } = context.exoharness.current;
  const protocolLog = new CodexProtocolEventBuffer(context);
  const scope: CodexWarmTurnScope = { context, protocolLog, turnParent };
  const sessionKey = codexWarmSessionKey(context, modelBinding);
  const sandboxRuntime = codexSandboxRuntimeKey(context);
  const { resource: session, reused: appServerReused } = await traceCodexTask(
    turnParent,
    "codex_app_server_ready",
    {
      cwd: codexAppServerCwd(context),
      sandbox_process: true,
      sandbox_runtime: sandboxRuntime,
      warm_session_key: sessionKey,
    },
    () =>
      codexSessions.get(sessionKey, () =>
        CodexWarmSession.start(scope, modelBinding, sessionKey),
      ),
  );
  session.setTurnScope(scope);

  const traceState: CodexTurnTraceState = {
    finalText: "",
    ttftMs: null,
    tokenUsage: null,
    promptMessages: [],
    startedAt: Date.now(),
    sawTextDelta: false,
  };

  try {
    const threadReused = session.threadId !== null;
    const threadId =
      session.threadId ??
      (await traceCodexTask(
        turnParent,
        "codex_thread_start",
        {
          runtime: "codex_app_server",
          model: modelBinding.model,
          cwd: codexAppServerCwd(context),
          external_sandbox: useCodexExternalSandbox(),
        },
        () => startCodexThread(session.server, context, modelBinding),
      ));
    session.threadId = threadId;
    await recordCodexWarmSession(
      context,
      sessionKey,
      session.process,
      threadId,
    );

    const priorInjection = threadReused
      ? emptyPriorResponseItems()
      : messagesToResponseItems(
          await materializePriorConversationMessages(context),
        );
    const priorItems = priorInjection.items;
    if (priorItems.length > 0) {
      await traceCodexTask(
        turnParent,
        "codex_thread_inject_items",
        {
          thread_id: threadId,
          item_count: priorItems.length,
          source_message_count: priorInjection.sourceMessageCount,
          dropped_message_count: priorInjection.droppedMessageCount,
          truncated_message_count: priorInjection.truncatedMessageCount,
          text_chars: priorInjection.textChars,
        },
        () =>
          session.server.request("thread/inject_items", {
            threadId,
            items: priorItems,
          }),
      );
    }

    const turnInput = messagesToUserInput(context.request.input);
    const turnStart = await traceCodexTask(
      turnParent,
      "codex_turn_start",
      {
        thread_id: threadId,
        model: modelBinding.model,
        input: turnInput,
        external_sandbox: useCodexExternalSandbox(),
      },
      () =>
        session.server.request<JsonObject>("turn/start", {
          threadId,
          input: turnInput,
          model: modelBinding.model,
          approvalPolicy: "on-request",
          sandboxPolicy: codexNativeSandboxPolicy(),
        }),
    );
    await appendCustomEvent(turn, "codex_turn_started", {
      metadata: turnMetadata(context),
      codex_thread_id: threadId,
      codex_turn: turnStart.turn ?? null,
      hydrated_from: threadReused ? "warm_codex_thread" : "exoharness_events",
      injected_response_items: priorItems.length,
      warm_app_server_reused: appServerReused,
      warm_thread_reused: threadReused,
    });

    await traceCodexLlmTurn(
      turnParent,
      context,
      threadId,
      priorItems.length,
      traceState,
      modelBinding,
      async () => {
        let completed = false;
        const activeItems = new Set<string>();
        for await (const notification of session.server.events()) {
          const outcome = await handleCodexNotification(
            context,
            turnParent,
            notification,
            activeItems,
            traceState,
          );
          if (outcome === "completed") {
            completed = true;
            break;
          }
        }

        if (!completed) {
          throw new Error("codex app-server stopped before turn completed");
        }
      },
    );

    await protocolLog.flush();
    return null;
  } catch (error) {
    await codexSessions.delete(sessionKey, (warmSession) => {
      warmSession.close();
    });
    throw error;
  } finally {
    session.clearTurnScope(scope);
    await protocolLog.flush();
  }
}

async function traceCodexTask<R>(
  turnParent: TraceParent,
  name: string,
  input: unknown,
  run: () => Promise<R>,
): Promise<R> {
  return tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        const result = await run();
        span.log({ output: traceOutputPreview(result) });
        return result;
      } catch (error) {
        span.log({ error: errorMessage(error) });
        throw error;
      }
    },
    {
      name,
      type: "task",
      spanAttributes: { purpose: "codex_app_server" },
      event: { input },
    },
  );
}

async function traceCodexLlmTurn(
  turnParent: TraceParent,
  context: TurnContext,
  threadId: string,
  injectedResponseItems: number,
  traceState: CodexTurnTraceState,
  modelBinding: ResolvedLlmBinding,
  run: () => Promise<void>,
): Promise<void> {
  await tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        await run();
        span.log({
          input: codexLlmTraceInput(context, traceState),
          output: codexLlmTraceOutput(traceState),
          metrics: codexUsageMetrics(traceState),
        });
      } catch (error) {
        span.log({ error: errorMessage(error) });
        throw error;
      }
    },
    {
      name: `codex:${modelBinding.model}`,
      type: "llm",
      spanAttributes: { purpose: "codex_llm_turn" },
      event: {
        input: context.request.input,
        metadata: {
          ...turnMetadata(context),
          runtime: "codex_app_server",
          model: modelBinding.model,
          codex_thread_id: threadId,
          injected_response_items: injectedResponseItems,
          streamed: context.streaming,
        },
      },
    },
  );
}

function traceOutputPreview(value: unknown): unknown {
  if (typeof value === "string") {
    return value;
  }
  if (isRecord(value) && "resource" in value && "reused" in value) {
    return { reused: value.reused };
  }
  if (isRecord(value)) {
    return value;
  }
  return null;
}

function codexUsageMetrics(
  traceState: CodexTurnTraceState,
): Record<string, number> {
  const metrics: Record<string, number> = {};
  const usage = traceState.tokenUsage;
  if (usage?.inputTokens !== undefined) {
    metrics.prompt_tokens = usage.inputTokens;
  }
  if (usage?.outputTokens !== undefined) {
    metrics.completion_tokens = usage.outputTokens;
  }
  if (usage?.totalTokens !== undefined) {
    metrics.tokens = usage.totalTokens;
  }
  if (usage?.cachedInputTokens !== undefined) {
    metrics.prompt_cached_tokens = usage.cachedInputTokens;
  }
  if (usage?.reasoningOutputTokens !== undefined) {
    metrics.completion_reasoning_tokens = usage.reasoningOutputTokens;
  }
  if (traceState.ttftMs !== null) {
    metrics.time_to_first_token = traceState.ttftMs / 1000;
  }
  return metrics;
}

function codexLlmTraceInput(
  context: TurnContext,
  traceState: CodexTurnTraceState,
): Message[] {
  return traceState.promptMessages.length > 0
    ? traceState.promptMessages
    : context.request.input;
}

function codexLlmTraceOutput(
  traceState: CodexTurnTraceState,
): Record<string, unknown> {
  return {
    messages: traceState.finalText
      ? [assistantTextMessage(traceState.finalText)]
      : [],
    tool_calls: [],
    status: "completed",
  };
}

async function startCodexThread(
  codex: CodexAppServer,
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): Promise<string> {
  const developerInstructions = codexDeveloperInstructions(context);
  const request: JsonObject = {
    model: modelBinding.model,
    modelProvider: "openai",
    cwd: codexAppServerCwd(context),
    approvalPolicy: "on-request",
    sandbox: "read-only",
    dynamicTools: buildCodexDynamicTools(context),
    ephemeral: true,
    experimentalRawEvents: true,
    persistFullHistory: true,
  };
  if (developerInstructions) {
    request.developerInstructions = developerInstructions;
  }
  const response = await codex.request<JsonObject>("thread/start", request);
  const thread = response.thread;
  if (!isRecord(thread) || typeof thread.id !== "string") {
    throw new Error("codex thread/start response did not include thread.id");
  }
  return thread.id;
}

async function latestCodexWarmSession(
  context: TurnContext,
  sessionKey: string,
  process: SandboxProcess,
): Promise<CodexWarmSessionRecord | null> {
  const result = await context.exoharness.current.conversation.getEvents({
    direction: "desc",
    limit: 100,
    types: [CODEX_WARM_SESSION_EVENT],
  });
  for (const event of result.events) {
    const record = codexWarmSessionRecord(event.data);
    if (
      record?.sessionKey === sessionKey &&
      (!process.sandboxId || record.sandboxId === process.sandboxId) &&
      (!process.sandboxProcessId ||
        record.sandboxProcessId === process.sandboxProcessId)
    ) {
      return record;
    }
  }
  return null;
}

async function recordCodexWarmSession(
  context: TurnContext,
  sessionKey: string,
  process: SandboxProcess,
  threadId: string,
): Promise<void> {
  await appendCustomEvent(
    context.exoharness.current.turn,
    CODEX_WARM_SESSION_EVENT,
    {
      sessionKey,
      sandboxId: process.sandboxId ?? null,
      sandboxProcessId: process.sandboxProcessId ?? null,
      threadId,
    },
  );
}

function codexWarmSessionRecord(
  data: EventData,
): CodexWarmSessionRecord | null {
  if (data.type !== "custom" || data.event_type !== CODEX_WARM_SESSION_EVENT) {
    return null;
  }
  const payload = data.payload;
  if (!isRecord(payload)) {
    return null;
  }
  const sessionKey = payload.sessionKey;
  const threadId = payload.threadId;
  if (typeof sessionKey !== "string" || typeof threadId !== "string") {
    return null;
  }
  const sandboxId =
    typeof payload.sandboxId === "string" ? payload.sandboxId : null;
  const sandboxProcessId =
    typeof payload.sandboxProcessId === "string"
      ? payload.sandboxProcessId
      : null;
  return {
    sessionKey,
    sandboxId,
    sandboxProcessId,
    threadId,
  };
}

async function handleCodexServerRequest(
  context: TurnContext,
  turnParent: TraceParent,
  request: CodexServerRequest,
): Promise<JsonValue | undefined> {
  if (
    useCodexExternalSandbox() &&
    request.method === "item/commandExecution/requestApproval"
  ) {
    return { decision: "accept" };
  }
  if (request.method !== "item/tool/call") {
    return undefined;
  }
  return executeDynamicToolCall(context, turnParent, asRecord(request.params));
}

async function executeDynamicToolCall(
  context: TurnContext,
  turnParent: TraceParent,
  params: Record<string, unknown>,
): Promise<JsonValue> {
  const callId = stringOrNull(params.callId) ?? "dynamic-tool-call";
  const toolName = stringOrNull(params.tool);
  if (toolName !== EXO_SHELL_DYNAMIC_TOOL) {
    return dynamicToolErrorResponse(`unsupported dynamic tool: ${toolName}`);
  }

  const args = asRecord(params.arguments);
  const command = stringOrNull(args.command);
  if (!command) {
    return dynamicToolErrorResponse("exo_shell requires a command string");
  }

  const toolCall: PendingToolCall = {
    toolCallId: callId,
    request: {
      functionName: EXO_SHELL_TOOL,
      arguments: objectArgs({ command }),
    },
  };
  await appendEvents(context, [toolRequestedEvent(toolCall)]);

  try {
    const result = await traceExoharnessToolCall(
      context,
      turnParent,
      toolCall,
      "codex_dynamic_tool",
    );
    await appendEvents(context, [toolResultEvent(callId, result)]);
    return dynamicToolResultResponse(shellToolResultText(result), {
      success: shellToolSucceeded(result),
    });
  } catch (error) {
    const message = errorMessage(error);
    await appendEvents(context, [
      toolResultEvent(callId, toJsonValue({ error: message })),
    ]);
    return dynamicToolErrorResponse(message);
  }
}

async function handleCodexNotification(
  context: TurnContext,
  turnParent: TraceParent,
  notification: CodexNotification,
  activeItems: Set<string>,
  traceState: CodexTurnTraceState,
): Promise<"running" | "completed"> {
  const { turn } = context.exoharness.current;
  updateTraceStateFromNotification(notification, traceState);
  switch (notification.method) {
    case "rawResponseItem/completed": {
      const params = asRecord(notification.params);
      const item = toJsonValue(params.item);
      await appendCustomEvent(turn, "codex_raw_response_item", {
        thread_id: params.threadId ?? null,
        turn_id: params.turnId ?? null,
        item,
      });
      return "running";
    }
    case "item/agentMessage/delta": {
      const params = asRecord(notification.params);
      if (typeof params.delta === "string") {
        const ttftMs = markFirstTextDelta(traceState);
        if (ttftMs !== null) {
          await context.stream.firstChunk(ttftMs);
        }
        await context.stream.text(params.delta);
      }
      return "running";
    }
    case "item/started": {
      const item = notificationItem(notification);
      const events = projectStartedItem(item, activeItems);
      await appendEvents(context, events);
      return "running";
    }
    case "item/completed": {
      const item = notificationItem(notification);
      const events = projectCompletedItem(item, activeItems);
      await appendEvents(context, events);
      const itemId = itemIdFromItem(item);
      const toolCall = itemId ? toolCallFromCodexItem(item, itemId) : null;
      if (toolCall) {
        await traceObservedToolCall(
          context,
          turnParent,
          toolCall,
          toolResultFromCodexItem(item),
          "codex_observed_tool",
        );
      }
      return "running";
    }
    case "turn/plan/updated":
      await appendCustomEvent(turn, "codex_plan_updated", notification.params);
      return "running";
    case "turn/diff/updated":
      await appendCustomEvent(turn, "codex_diff_updated", notification.params);
      return "running";
    case "thread/tokenUsage/updated":
      await appendCustomEvent(turn, "codex_token_usage", notification.params);
      return "running";
    case "turn/completed": {
      await appendCustomEvent(
        turn,
        "codex_turn_completed",
        notification.params,
      );
      const params = asRecord(notification.params);
      const completedTurn = asRecord(params.turn);
      const status = completedTurn.status;
      if (status === "failed") {
        const message = codexTurnError(completedTurn);
        await streamCodexStatus(context, traceState, `error: ${message}`);
        throw new Error(message);
      }
      return "completed";
    }
    case "error": {
      const params = asRecord(notification.params);
      const message = codexNotificationError(params);
      await appendCustomEvent(
        turn,
        params.willRetry === true ? "codex_retrying" : "codex_error",
        notification.params,
      );
      if (params.willRetry === true && !codexEffectiveNetworking(context)) {
        throw new Error(codexSandboxNetworkingError(context));
      }
      if (params.willRetry === true) {
        await streamCodexStatus(context, traceState, `retrying: ${message}`);
        return "running";
      }
      await streamCodexStatus(context, traceState, `error: ${message}`);
      throw new Error(message);
    }
    default:
      return "running";
  }
}

function projectStartedItem(
  item: Record<string, unknown>,
  activeItems: Set<string>,
): EventData[] {
  const itemId = itemIdFromItem(item);
  if (!itemId) {
    return [];
  }
  const toolCall = toolCallFromCodexItem(item, itemId);
  if (!toolCall) {
    return [];
  }
  activeItems.add(itemId);
  return [toolRequestedEvent(toolCall)];
}

function projectCompletedItem(
  item: Record<string, unknown>,
  activeItems: Set<string>,
): EventData[] {
  const type = item.type;
  if (type === "agentMessage" && typeof item.text === "string") {
    return [messagesEvent([assistantTextMessage(item.text)])];
  }

  const itemId = itemIdFromItem(item);
  if (!itemId) {
    return [customItemEvent("codex_item_completed", item)];
  }

  const events: EventData[] = [];
  const toolCall = toolCallFromCodexItem(item, itemId);
  if (toolCall && !activeItems.has(itemId)) {
    events.push(toolRequestedEvent(toolCall));
  }
  if (toolCall) {
    events.push(toolResultEvent(itemId, toolResultFromCodexItem(item)));
    activeItems.delete(itemId);
    return events;
  }

  if (type === "fileChange") {
    return [customItemEvent("codex_file_change", item)];
  }
  if (type === "reasoning") {
    return [customItemEvent("codex_reasoning", item)];
  }
  if (type === "todoList") {
    return [customItemEvent("codex_todo_list", item)];
  }
  return [customItemEvent("codex_item_completed", item)];
}

function toolCallFromCodexItem(
  item: Record<string, unknown>,
  itemId: string,
): PendingToolCall | null {
  if (item.type === "commandExecution" && typeof item.command === "string") {
    return {
      toolCallId: itemId,
      request: {
        functionName: CODEX_SHELL_TOOL,
        arguments: objectArgs({
          command: item.command,
          cwd: stringOrNull(item.cwd),
        }),
      },
    };
  }

  if (item.type === "mcpToolCall") {
    return {
      toolCallId: itemId,
      request: {
        functionName: `codex.mcp.${String(item.server ?? "unknown")}.${String(item.tool ?? "unknown")}`,
        arguments: objectArgs({
          server: stringOrNull(item.server),
          tool: stringOrNull(item.tool),
          arguments: toJsonValue(item.arguments ?? null),
        }),
      },
    };
  }

  if (item.type === "webSearch") {
    return {
      toolCallId: itemId,
      request: {
        functionName: CODEX_WEB_SEARCH_TOOL,
        arguments: objectArgs({
          query: stringOrNull(item.query),
          action: toJsonValue(item.action ?? null),
        }),
      },
    };
  }

  return null;
}

function toolResultFromCodexItem(item: Record<string, unknown>): JsonValue {
  if (item.type === "commandExecution") {
    return toJsonValue({
      status: item.status ?? null,
      exit_code: item.exitCode ?? null,
      output: item.aggregatedOutput ?? null,
      duration_ms: item.durationMs ?? null,
    });
  }
  if (item.type === "mcpToolCall") {
    return toJsonValue({
      status: item.status ?? null,
      result: item.result ?? null,
      error: item.error ?? null,
    });
  }
  return toJsonValue({
    status: item.status ?? null,
    result: item,
  });
}

function emptyPriorResponseItems(): PriorResponseItems {
  return {
    items: [],
    sourceMessageCount: 0,
    droppedMessageCount: 0,
    truncatedMessageCount: 0,
    textChars: 0,
  };
}

function messagesToResponseItems(messages: Message[]): PriorResponseItems {
  const candidates = messages
    .filter(
      (message) => message.role !== "system" && message.role !== "developer",
    )
    .map(priorMessageToResponseItemCandidate);
  const selected: PriorResponseItemCandidate[] = [];
  let textChars = 0;
  let droppedMessageCount = 0;

  for (let index = candidates.length - 1; index >= 0; index -= 1) {
    const candidate = candidates[index];
    if (
      selected.length > 0 &&
      textChars + candidate.textChars > CODEX_PRIOR_HISTORY_MAX_CHARS
    ) {
      droppedMessageCount += 1;
      continue;
    }
    selected.push(candidate);
    textChars += candidate.textChars;
  }

  selected.reverse();
  return {
    items: selected.map((candidate) => candidate.item),
    sourceMessageCount: candidates.length,
    droppedMessageCount,
    truncatedMessageCount: candidates.filter((candidate) => candidate.truncated)
      .length,
    textChars,
  };
}

function priorMessageToResponseItemCandidate(
  message: Message,
): PriorResponseItemCandidate {
  const { text, truncated } = truncatePriorMessageText(
    messageText(message),
    priorMessageMaxChars(message),
  );
  if (message.role === "assistant") {
    return {
      item: toJsonValue({
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text }],
      }),
      textChars: text.length,
      truncated,
    };
  }
  return {
    item: toJsonValue({
      type: "message",
      role: "user",
      content: [{ type: "input_text", text }],
    }),
    textChars: text.length,
    truncated,
  };
}

function priorMessageMaxChars(message: Message): number {
  return message.role === "tool"
    ? CODEX_PRIOR_TOOL_RESULT_MAX_CHARS
    : CODEX_PRIOR_MESSAGE_MAX_CHARS;
}

function truncatePriorMessageText(
  text: string,
  maxChars: number,
): { text: string; truncated: boolean } {
  if (text.length <= maxChars) {
    return { text, truncated: false };
  }
  const omittedChars = text.length - maxChars;
  const suffix = `\n\n[truncated ${omittedChars} characters from prior conversation history]`;
  return {
    text: `${text.slice(0, Math.max(0, maxChars - suffix.length))}${suffix}`,
    truncated: true,
  };
}

function messagesToUserInput(messages: Message[]): JsonValue[] {
  const text = messages
    .filter((message) => message.role === "user")
    .map(messageText)
    .join("\n\n");
  return [
    {
      type: "text",
      text: text || messages.map(messageText).join("\n\n"),
      text_elements: [],
    },
  ];
}

function codexDeveloperInstructions(context: TurnContext): string | null {
  return instructionsText(context.agentConfig.instructions) || null;
}

function buildCodexDynamicTools(context: TurnContext): JsonValue[] {
  if (useCodexExternalSandbox()) {
    return [];
  }
  if (!context.conversationConfig.shellProgram) {
    return [];
  }
  return [
    {
      name: EXO_SHELL_DYNAMIC_TOOL,
      description: `Run a shell command through the exoharness sandbox. Commands execute from ${sandboxCwd(context)}. Use this for command execution in exo conversations.`,
      inputSchema: {
        type: "object",
        additionalProperties: false,
        properties: {
          command: {
            type: "string",
            description: "Shell command to execute.",
          },
        },
        required: ["command"],
      },
    },
  ];
}

function codexNativeSandboxPolicy(): JsonValue {
  if (useCodexExternalSandbox()) {
    return {
      type: "externalSandbox",
      networkAccess: "restricted",
    };
  }
  return {
    type: "readOnly",
    networkAccess: false,
  };
}

function useCodexExternalSandbox(): boolean {
  return true;
}

async function requireCodexSandboxNetworking(
  context: TurnContext,
): Promise<void> {
  if (codexEffectiveNetworking(context)) {
    return;
  }
  await appendCustomEvent(
    context.exoharness.current.turn,
    "codex_networking_required",
    {
      metadata: turnMetadata(context),
      agent_enable_networking: context.agentConfig.sandbox.enableNetworking,
      reason:
        "Codex runs its model stream inside the exoharness sandbox, so the agent sandbox must have networking enabled.",
    },
  );
  throw new Error(codexSandboxNetworkingError(context));
}

function codexSandboxNetworkingError(context: TurnContext): string {
  return [
    "Codex requires agent networking because it runs model calls inside the exoharness sandbox.",
    `Enable it with: exo agent update ${context.exoharness.current.agent.record.slug} --networking enabled`,
  ].join(" ");
}

function codexEffectiveNetworking(context: TurnContext): boolean {
  return context.agentConfig.sandbox.enableNetworking;
}

function codexSandboxCommand(context: TurnContext): string[] {
  const shell = context.conversationConfig.shellProgram ?? "/bin/bash";
  const command = [
    "set -e;",
    'mkdir -p "${HOME:-/tmp/exo-home}" "${CODEX_HOME:-/tmp/exo-codex-home}" >/dev/null 2>/tmp/codex-setup.stderr;',
    'if [ -n "${OPENAI_API_KEY:-}" ] && [ ! -f "${CODEX_HOME:-/tmp/exo-codex-home}/auth.json" ]; then',
    'printf "%s" "$OPENAI_API_KEY" | codex login --with-api-key >/dev/null 2>/tmp/codex-login.stderr;',
    "fi;",
    "exec codex app-server --listen stdio:// 2>/tmp/codex-app-server.stderr",
  ].join(" ");
  return [shell, "-lc", command];
}

function codexSandboxEnv(
  modelBinding: ResolvedLlmBinding,
): Record<string, string> {
  const env: Record<string, string> = {
    ...pickEnv(
      (key) =>
        [
          "BRAINTRUST_API_KEY",
          "BRAINTRUST_APP_URL",
          "OPENAI_ORG_ID",
          "OPENAI_ORGANIZATION",
          "OPENAI_PROJECT",
        ].includes(key) || key.startsWith("CODEX_"),
    ),
    CODEX_HOME: "/tmp/exo-codex-home",
    HOME: "/tmp/exo-home",
  };
  if (modelBinding.apiKey) {
    env.OPENAI_API_KEY = modelBinding.apiKey;
  }
  if (modelBinding.baseUrl) {
    env.OPENAI_BASE_URL = modelBinding.baseUrl;
  }
  return env;
}

function dynamicToolResultResponse(
  text: string,
  options: { success: boolean },
): JsonValue {
  return toJsonValue({
    contentItems: [{ type: "inputText", text }],
    success: options.success,
  });
}

function dynamicToolErrorResponse(message: string): JsonValue {
  return dynamicToolResultResponse(`Error: ${message}`, { success: false });
}

function codexAppServerCwd(context: TurnContext): string {
  return sandboxCwd(context);
}

function codexEffectiveSandboxProvider(
  context: TurnContext,
): TurnContext["agentConfig"]["sandbox"]["provider"] {
  return (
    context.conversationConfig.sandboxProvider ??
    context.agentConfig.sandbox.provider
  );
}

function codexEffectiveSandboxImage(context: TurnContext): string | null {
  return (
    context.conversationConfig.sandboxImage ??
    context.agentConfig.sandbox.image ??
    null
  );
}

function codexSandboxRuntimeKey(context: TurnContext): JsonValue {
  return {
    provider: codexEffectiveSandboxProvider(context),
    image: codexEffectiveSandboxImage(context),
    enable_networking: codexEffectiveNetworking(context),
    cwd: codexAppServerCwd(context),
    shell_program: context.conversationConfig.shellProgram ?? "/bin/bash",
    mounts: context.conversationConfig.mounts.map((mount) => ({
      host_path: mount.hostPath,
      mount_path: mount.mountPath,
      mode: mount.mode,
      internal: mount.internal ?? false,
    })),
    command: codexSandboxCommand(context),
    external_sandbox: useCodexExternalSandbox(),
  };
}

function codexWarmSessionKey(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): string {
  return JSON.stringify({
    agent_id: context.exoharness.current.agent.record.id,
    conversation_id: context.exoharness.current.conversation.record.id,
    model_binding: modelBinding.name,
    model: modelBinding.model,
    base_url: modelBinding.baseUrl ?? null,
    sandbox_runtime: codexSandboxRuntimeKey(context),
  });
}

function notificationItem(
  notification: CodexNotification,
): Record<string, unknown> {
  const params = asRecord(notification.params);
  return asRecord(params.item);
}

function itemIdFromItem(item: Record<string, unknown>): string | null {
  return typeof item.id === "string" ? item.id : null;
}

function customItemEvent(
  eventType: string,
  item: Record<string, unknown>,
): EventData {
  return {
    type: "custom",
    event_type: eventType,
    payload: toJsonValue(item),
  };
}

function codexTurnError(turn: Record<string, unknown>): string {
  const error = turn.error;
  if (isRecord(error) && typeof error.message === "string") {
    const details =
      typeof error.additionalDetails === "string"
        ? ` (${error.additionalDetails})`
        : "";
    return `${error.message}${details}`;
  }
  return "codex turn failed";
}

function codexNotificationError(params: Record<string, unknown>): string {
  const error = asRecord(params.error);
  const message =
    typeof error.message === "string" ? error.message : stringifyValue(params);
  const details =
    typeof error.additionalDetails === "string"
      ? ` (${error.additionalDetails})`
      : "";
  return `${message}${details}`;
}

async function streamCodexStatus(
  context: TurnContext,
  traceState: CodexTurnTraceState,
  message: string,
): Promise<void> {
  const ttftMs = markFirstTextDelta(traceState);
  if (ttftMs !== null) {
    await context.stream.firstChunk(ttftMs);
  }
  await context.stream.text(`[codex] ${message}\n`);
}

function updateTraceStateFromNotification(
  notification: CodexNotification,
  traceState: CodexTurnTraceState,
): void {
  if (notification.method === "rawResponseItem/completed") {
    const params = asRecord(notification.params);
    const item = asRecord(params.item);
    const message = rawCodexPromptMessage(item);
    if (message) {
      traceState.promptMessages.push(message);
    }
    return;
  }
  if (notification.method === "item/agentMessage/delta") {
    const params = asRecord(notification.params);
    if (typeof params.delta === "string") {
      traceState.finalText += params.delta;
    }
    return;
  }
  if (notification.method === "item/completed") {
    const item = notificationItem(notification);
    if (item.type === "agentMessage" && typeof item.text === "string") {
      traceState.finalText = item.text;
    }
    return;
  }
  if (notification.method === "thread/tokenUsage/updated") {
    const params = asRecord(notification.params);
    const tokenUsage = asRecord(params.tokenUsage);
    const last = asRecord(tokenUsage.last);
    traceState.tokenUsage = {
      inputTokens: numberField(last.inputTokens),
      outputTokens: numberField(last.outputTokens),
      totalTokens: numberField(last.totalTokens),
      cachedInputTokens: numberField(last.cachedInputTokens),
      reasoningOutputTokens: numberField(last.reasoningOutputTokens),
    };
  }
}

function rawCodexPromptMessage(item: Record<string, unknown>): Message | null {
  if (item.type !== "message") {
    return null;
  }
  if (!isRawCodexPromptRole(item.role)) {
    return null;
  }
  const messages = responsesMessagesToLingua([item]) as Message[];
  return messages[0] ?? null;
}

function isRawCodexPromptRole(value: unknown): boolean {
  return value === "system" || value === "developer" || value === "user";
}

class CodexProtocolEventBuffer {
  private readonly context: TurnContext;
  private readonly entries: CodexProtocolLogEntry[] = [];

  constructor(context: TurnContext) {
    this.context = context;
  }

  record(entry: CodexProtocolLogEntry): void {
    if (isCodexProtocolDelta(entry.message)) {
      return;
    }
    this.entries.push(entry);
  }

  async flush(): Promise<void> {
    if (this.entries.length === 0) {
      return;
    }
    const entries = this.entries.splice(0);
    const metadata = turnMetadata(this.context);
    await this.context.exoharness.current.turn.addEvents(
      entries.map((entry) => ({
        type: "custom",
        event_type: "codex_protocol_message",
        payload: toJsonValue({
          ...entry,
          metadata,
        }),
      })),
    );
  }
}

function isCodexProtocolDelta(message: JsonValue): boolean {
  if (!isRecord(message)) {
    return false;
  }
  const method = message.method;
  return typeof method === "string" && method.endsWith("/delta");
}
