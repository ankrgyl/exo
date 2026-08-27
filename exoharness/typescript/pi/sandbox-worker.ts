import { mkdir } from "node:fs/promises";
import readline from "node:readline/promises";

import {
  createAgentSession,
  createExtensionRuntime,
  ModelRuntime,
  SessionManager,
  SettingsManager,
  type AgentSessionEvent,
  type ResourceLoader,
} from "@earendil-works/pi-coding-agent";

import {
  parsePiModelReference,
  toPiJson,
  type PiUsage,
  type PiWorkerEvent,
  type PiWorkerRequest,
  type PiWorkerRunResult,
} from "./protocol";

const PI_AGENT_DIR =
  process.env.EXO_PI_AGENT_DIR ?? `/tmp/exo-pi-agent-${process.pid}`;
const PI_API_KEY = process.env.EXO_PI_API_KEY;
delete process.env.EXO_PI_API_KEY;

const PI_TOOLS = ["read", "bash", "edit", "write", "grep", "find", "ls"];

interface PiSessionTiming {
  assistantStartedAt: number | null;
  assistantTtftMs: number | null;
}

async function main(): Promise<void> {
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  try {
    for await (const line of rl) {
      if (!line.trim()) {
        continue;
      }
      let requestId = "unknown";
      try {
        const request = parseRequest(line);
        requestId = request.requestId;
        writeEvent({ type: "run_started", requestId });
        await handleRequest(request);
      } catch (error) {
        writeEvent({
          type: "error",
          requestId,
          message: errorMessage(error),
          error: toPiJson(error),
        });
      }
    }
  } finally {
    rl.close();
  }
}

async function handleRequest(request: PiWorkerRequest): Promise<void> {
  const startedAt = Date.now();
  const { provider, model: modelId } = parsePiModelReference(request.model);
  await mkdir(PI_AGENT_DIR, { recursive: true });
  const modelRuntime = await ModelRuntime.create({
    authPath: `${PI_AGENT_DIR}/auth.json`,
    modelsPath: null,
    refreshOnCreate: false,
  });
  if (PI_API_KEY) {
    await modelRuntime.setRuntimeApiKey(provider, PI_API_KEY);
  }

  const registeredModel = modelRuntime.getModel(provider, modelId);
  if (!registeredModel) {
    throw new Error(
      `Pi does not know model ${request.model}; use a provider/model reference from Pi's built-in model catalog`,
    );
  }
  const model = {
    ...registeredModel,
    ...(request.baseUrl ? { baseUrl: request.baseUrl } : {}),
    ...(request.maxOutputTokens !== undefined
      ? {
          maxTokens: Math.min(
            registeredModel.maxTokens,
            request.maxOutputTokens,
          ),
        }
      : {}),
  };
  const settingsManager = SettingsManager.inMemory({
    compaction: { enabled: true },
    retry: { enabled: true, maxRetries: 2 },
  });
  const resourceLoader = isolatedResourceLoader(request.systemPrompt);
  const { session } = await createAgentSession({
    cwd: request.cwd,
    agentDir: PI_AGENT_DIR,
    model,
    modelRuntime,
    tools: PI_TOOLS,
    resourceLoader,
    sessionManager: SessionManager.inMemory(request.cwd),
    settingsManager,
  });

  installRoundBudget(session.agent, request.maxToolRoundTrips);
  const timing: PiSessionTiming = {
    assistantStartedAt: null,
    assistantTtftMs: null,
  };
  const unsubscribe = session.subscribe((event) =>
    handleSessionEvent(request.requestId, event, timing),
  );
  try {
    await session.prompt(request.prompt, { expandPromptTemplates: false });
    const finalMessage = [...session.messages]
      .reverse()
      .find((message) => message.role === "assistant");
    const finalError =
      finalMessage?.role !== "assistant"
        ? "Pi produced no final assistant message"
        : finalMessage.stopReason === "error" ||
            finalMessage.stopReason === "aborted"
          ? (finalMessage.errorMessage ??
            `Pi stopped: ${finalMessage.stopReason}`)
          : undefined;
    const result: PiWorkerRunResult = {
      status: finalError ? "error" : "finished",
      finalText:
        finalMessage?.role === "assistant"
          ? assistantText(finalMessage.content)
          : "",
      model: model.id,
      provider: model.provider,
      usage: aggregateUsage(session.messages),
      durationMs: Date.now() - startedAt,
      error: finalError,
    };
    writeEvent({ type: "completed", requestId: request.requestId, result });
  } finally {
    unsubscribe();
    session.dispose();
  }
}

function handleSessionEvent(
  requestId: string,
  event: AgentSessionEvent,
  timing: PiSessionTiming,
): void {
  if (event.type === "message_start" && event.message.role === "assistant") {
    timing.assistantStartedAt = Date.now();
    timing.assistantTtftMs = null;
    return;
  }
  if (
    event.type === "message_update" &&
    event.assistantMessageEvent.type === "text_delta"
  ) {
    if (timing.assistantTtftMs === null && timing.assistantStartedAt !== null) {
      timing.assistantTtftMs = Date.now() - timing.assistantStartedAt;
    }
    writeEvent({
      type: "delta",
      requestId,
      text: event.assistantMessageEvent.delta,
    });
    return;
  }
  if (event.type === "message_end" && event.message.role === "assistant") {
    const durationMs =
      timing.assistantStartedAt === null
        ? undefined
        : Date.now() - timing.assistantStartedAt;
    writeEvent({
      type: "message",
      requestId,
      message: toPiJson(event.message),
      durationMs,
      ttftMs: timing.assistantTtftMs ?? undefined,
    });
    timing.assistantStartedAt = null;
    timing.assistantTtftMs = null;
    return;
  }
  if (event.type === "message_end" && event.message.role === "toolResult") {
    writeEvent({
      type: "message",
      requestId,
      message: toPiJson(event.message),
    });
    return;
  }
  if (event.type === "tool_execution_start") {
    writeEvent({
      type: "tool_start",
      requestId,
      callId: event.toolCallId,
      name: event.toolName,
      args: toPiJson(event.args),
    });
    return;
  }
  if (event.type === "tool_execution_end") {
    writeEvent({
      type: "tool_end",
      requestId,
      callId: event.toolCallId,
      name: event.toolName,
      result: toPiJson(event.result),
      isError: event.isError,
    });
    return;
  }
  if (event.type === "auto_retry_start" || event.type === "auto_retry_end") {
    writeEvent({
      type: "retry",
      requestId,
      phase: event.type === "auto_retry_start" ? "start" : "end",
      details: toPiJson(event),
    });
    return;
  }
  if (event.type === "compaction_start" || event.type === "compaction_end") {
    writeEvent({
      type: "compaction",
      requestId,
      phase: event.type === "compaction_start" ? "start" : "end",
      details: toPiJson(event),
    });
  }
}

function isolatedResourceLoader(systemPrompt: string): ResourceLoader {
  return {
    getExtensions: () => ({
      extensions: [],
      errors: [],
      runtime: createExtensionRuntime(),
    }),
    getSkills: () => ({ skills: [], diagnostics: [] }),
    getPrompts: () => ({ prompts: [], diagnostics: [] }),
    getThemes: () => ({ themes: [], diagnostics: [] }),
    getAgentsFiles: () => ({ agentsFiles: [] }),
    getSystemPrompt: () => undefined,
    getSystemPromptSource: () => undefined,
    getAppendSystemPrompt: () => [systemPrompt],
    getAppendSystemPromptSources: () => [],
    extendResources: () => {},
    reload: async () => {},
  };
}

function aggregateUsage(messages: readonly unknown[]): PiUsage {
  const usage: PiUsage = {
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    totalTokens: 0,
    cost: 0,
  };
  let reasoning = 0;
  let hasReasoning = false;
  for (const message of messages) {
    if (!isRecord(message) || message.role !== "assistant") {
      continue;
    }
    const item = isRecord(message.usage) ? message.usage : {};
    usage.input += numberValue(item.input);
    usage.output += numberValue(item.output);
    usage.cacheRead += numberValue(item.cacheRead);
    usage.cacheWrite += numberValue(item.cacheWrite);
    usage.totalTokens += numberValue(item.totalTokens);
    if (typeof item.reasoning === "number") {
      reasoning += item.reasoning;
      hasReasoning = true;
    }
    const cost = isRecord(item.cost) ? item.cost : {};
    usage.cost += numberValue(cost.total);
  }
  if (hasReasoning) {
    usage.reasoning = reasoning;
  }
  return usage;
}

function assistantText(content: readonly unknown[]): string {
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

function parseRequest(line: string): PiWorkerRequest {
  const parsed = JSON.parse(line) as unknown;
  if (!isRecord(parsed)) {
    throw new Error("Pi sandbox worker request must be a JSON object");
  }
  for (const field of [
    "requestId",
    "prompt",
    "systemPrompt",
    "model",
    "cwd",
  ] as const) {
    if (typeof parsed[field] !== "string" || !parsed[field]) {
      throw new Error(`Pi sandbox worker request requires ${field}`);
    }
  }
  const maxOutputTokens = optionalPositiveInteger(
    parsed.maxOutputTokens,
    "maxOutputTokens",
  );
  const maxToolRoundTrips = optionalNonNegativeInteger(
    parsed.maxToolRoundTrips,
    "maxToolRoundTrips",
  );
  return {
    requestId: parsed.requestId as string,
    prompt: parsed.prompt as string,
    systemPrompt: parsed.systemPrompt as string,
    model: parsed.model as string,
    cwd: parsed.cwd as string,
    baseUrl: typeof parsed.baseUrl === "string" ? parsed.baseUrl : undefined,
    maxOutputTokens,
    maxToolRoundTrips,
  };
}

function installRoundBudget(
  agent: Awaited<ReturnType<typeof createAgentSession>>["session"]["agent"],
  maxToolRoundTrips: number | undefined,
): void {
  if (maxToolRoundTrips === undefined) {
    return;
  }
  const previous = agent.shouldStopAfterTurn;
  let completedToolRoundTrips = 0;
  agent.shouldStopAfterTurn = async (context, signal) => {
    if (previous && (await previous(context, signal))) {
      return true;
    }
    const hasToolCalls = context.message.content.some(
      (part) => part.type === "toolCall",
    );
    if (!hasToolCalls) {
      return false;
    }
    const budgetExhausted = completedToolRoundTrips >= maxToolRoundTrips;
    completedToolRoundTrips += 1;
    return budgetExhausted;
  };
}

function optionalPositiveInteger(
  value: unknown,
  field: string,
): number | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
    throw new Error(`Pi sandbox worker ${field} must be a positive integer`);
  }
  return value;
}

function optionalNonNegativeInteger(
  value: unknown,
  field: string,
): number | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Error(
      `Pi sandbox worker ${field} must be a non-negative integer`,
    );
  }
  return value;
}

function writeEvent(event: PiWorkerEvent): void {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

function numberValue(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

void main().catch((error: unknown) => {
  writeEvent({
    type: "error",
    requestId: "worker",
    message: errorMessage(error),
    error: toPiJson(error),
  });
  process.exitCode = 1;
});
