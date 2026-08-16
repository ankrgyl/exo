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

const PI_AGENT_DIR = "/tmp/exo-pi-agent";
const PI_TOOLS = ["read", "bash", "edit", "write", "grep", "find", "ls"];

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
      try {
        await handleRequest(parseRequest(line));
      } catch (error) {
        writeEvent({
          type: "error",
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
  const { provider, model: modelId } = parsePiModelReference(request.model);
  await mkdir(PI_AGENT_DIR, { recursive: true });
  const modelRuntime = await ModelRuntime.create({
    authPath: `${PI_AGENT_DIR}/auth.json`,
    modelsPath: null,
    refreshOnCreate: false,
  });
  const apiKey = process.env.EXO_PI_API_KEY;
  if (apiKey) {
    await modelRuntime.setRuntimeApiKey(provider, apiKey);
  }

  const registeredModel = modelRuntime.getModel(provider, modelId);
  if (!registeredModel) {
    throw new Error(
      `Pi does not know model ${request.model}; use a provider/model reference from Pi's built-in model catalog`,
    );
  }
  const model = request.baseUrl
    ? { ...registeredModel, baseUrl: request.baseUrl }
    : registeredModel;
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

  const unsubscribe = session.subscribe(handleSessionEvent);
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
      error: finalError,
    };
    writeEvent({ type: "completed", result });
  } finally {
    unsubscribe();
    session.dispose();
  }
}

function handleSessionEvent(event: AgentSessionEvent): void {
  if (
    event.type === "message_update" &&
    event.assistantMessageEvent.type === "text_delta"
  ) {
    writeEvent({ type: "delta", text: event.assistantMessageEvent.delta });
    return;
  }
  if (
    event.type === "message_end" &&
    (event.message.role === "assistant" || event.message.role === "toolResult")
  ) {
    writeEvent({ type: "message", message: toPiJson(event.message) });
    return;
  }
  if (event.type === "tool_execution_start") {
    writeEvent({
      type: "tool_start",
      callId: event.toolCallId,
      name: event.toolName,
      args: toPiJson(event.args),
    });
    return;
  }
  if (event.type === "tool_execution_end") {
    writeEvent({
      type: "tool_end",
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
      phase: event.type === "auto_retry_start" ? "start" : "end",
      details: toPiJson(event),
    });
    return;
  }
  if (event.type === "compaction_start" || event.type === "compaction_end") {
    writeEvent({
      type: "compaction",
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
    const cost = isRecord(item.cost) ? item.cost : {};
    usage.cost += numberValue(cost.total);
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
  for (const field of ["prompt", "systemPrompt", "model", "cwd"] as const) {
    if (typeof parsed[field] !== "string") {
      throw new Error(`Pi sandbox worker request requires ${field}`);
    }
  }
  return {
    prompt: parsed.prompt as string,
    systemPrompt: parsed.systemPrompt as string,
    model: parsed.model as string,
    cwd: parsed.cwd as string,
    baseUrl: typeof parsed.baseUrl === "string" ? parsed.baseUrl : undefined,
  };
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
    message: errorMessage(error),
    error: toPiJson(error),
  });
  process.exitCode = 1;
});
