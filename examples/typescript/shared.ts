import {
  materializeEventsToMessages,
  messageText,
  stringifyValue,
  toJsonValue,
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
  errorMessage,
  modelProvider,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";

export { projectAnthropicMessageToolEvents } from "@exo/harness";

export interface TextDeltaTraceState {
  startedAt: number;
  sawTextDelta: boolean;
  ttftMs: number | null;
}

export interface ResolvedLlmBinding {
  name: string;
  model: string;
  provider: string;
  auth?: {
    authorization?: string;
    headers: Record<string, string>;
  };
  baseUrl?: string | null;
}

export class AsyncQueue<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{
    resolve: (result: IteratorResult<T>) => void;
    reject: (error: Error) => void;
  }> = [];
  private ended = false;
  private error: Error | null = null;

  push(value: T): void {
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve({ done: false, value });
      return;
    }
    this.values.push(value);
  }

  end(): void {
    this.ended = true;
    while (this.waiters.length > 0) {
      this.waiters.shift()?.resolve({ done: true, value: undefined });
    }
  }

  fail(error: Error): void {
    this.error = error;
    while (this.waiters.length > 0) {
      this.waiters.shift()?.reject(error);
    }
  }

  async next(): Promise<IteratorResult<T>> {
    if (this.error) {
      throw this.error;
    }
    const value = this.values.shift();
    if (value !== undefined) {
      return { done: false, value };
    }
    if (this.ended) {
      return { done: true, value: undefined };
    }
    return new Promise((resolve, reject) => {
      this.waiters.push({ resolve, reject });
    });
  }
}

export class WarmResourceCache<T> {
  private readonly entries = new Map<string, Promise<T>>();

  async get(
    key: string,
    create: () => Promise<T>,
  ): Promise<{ resource: T; reused: boolean }> {
    const existing = this.entries.get(key);
    if (existing) {
      return { resource: await existing, reused: true };
    }

    const created = create().catch((error: unknown) => {
      this.entries.delete(key);
      throw error;
    });
    this.entries.set(key, created);
    return { resource: await created, reused: false };
  }

  async delete(
    key: string,
    close?: (resource: T) => Promise<void> | void,
  ): Promise<void> {
    const existing = this.entries.get(key);
    this.entries.delete(key);
    if (!existing || !close) {
      return;
    }
    const resource = await existing;
    await close(resource);
  }
}

export interface WarmJsonlSandboxWorkerOptions<TEvent> {
  name: string;
  parseEvent(line: string): TEvent;
  process: SandboxProcess;
}

export class WarmJsonlSandboxWorker<TRequest, TEvent> {
  private readonly events = new AsyncQueue<TEvent>();
  private stderr = "";

  constructor(private readonly options: WarmJsonlSandboxWorkerOptions<TEvent>) {
    void this.readStdout();
    void this.readStderr();
  }

  async request<R>(
    request: TRequest,
    onEvent: (event: TEvent) => Promise<R | undefined> | R | undefined,
  ): Promise<R> {
    await this.options.process.writeStdin(`${JSON.stringify(request)}\n`);
    while (true) {
      const next = await this.events.next();
      if (next.done) {
        throw new Error(
          `${this.options.name} exited before completion${this.stderrSuffix()}`,
        );
      }
      const result = await onEvent(next.value);
      if (result !== undefined) {
        return result;
      }
    }
  }

  async close(): Promise<void> {
    await this.options.process.close();
  }

  private async readStdout(): Promise<void> {
    try {
      let buffered = "";
      const reader = this.options.process.stdout.getReader();
      try {
        for (;;) {
          const result = await reader.read();
          if (result.done) {
            this.pushLine(buffered);
            this.events.end();
            return;
          }
          buffered += result.value;
          while (true) {
            const newline = buffered.indexOf("\n");
            if (newline < 0) {
              break;
            }
            const line = buffered.slice(0, newline).replace(/\r$/, "");
            buffered = buffered.slice(newline + 1);
            this.pushLine(line);
          }
        }
      } finally {
        reader.releaseLock();
      }
    } catch (error) {
      this.events.fail(
        error instanceof Error ? error : new Error(String(error)),
      );
    }
  }

  private async readStderr(): Promise<void> {
    const reader = this.options.process.stderr.getReader();
    try {
      for (;;) {
        const result = await reader.read();
        if (result.done) {
          return;
        }
        this.stderr += result.value;
      }
    } catch (error) {
      this.stderr += `\nfailed to read stderr: ${
        error instanceof Error ? error.message : String(error)
      }`;
    } finally {
      reader.releaseLock();
    }
  }

  private pushLine(line: string): void {
    if (line.trim()) {
      this.events.push(this.options.parseEvent(line));
    }
  }

  private stderrSuffix(): string {
    const stderr = this.stderr.trim();
    if (!stderr) {
      return "";
    }
    const maxChars = 16_000;
    const preview =
      stderr.length > maxChars
        ? `${stderr.slice(0, maxChars)}\n... stderr truncated ...`
        : stderr;
    return `\nstderr:\n${preview}`;
  }
}

export async function resolveLlmBinding(
  context: TurnContext,
): Promise<ResolvedLlmBinding> {
  const name = context.agentConfig.model;
  const metadata = (
    await context.exoharness.current.conversation.listBindings()
  )
    .filter((binding) => binding.type === "llm")
    .filter((binding) => binding.name === name)
    .sort((left, right) => right.createdAt.localeCompare(left.createdAt))[0];
  if (!metadata) {
    throw new Error(
      `model is not registered: ${name}; run \`exo model register ${name} --secret <secret>\``,
    );
  }
  const binding = await context.exoharness.current.conversation.getBinding(
    metadata.id,
  );
  if (!binding || binding.type !== "llm") {
    throw new Error(`registered model binding disappeared: ${name}`);
  }
  const provider = modelProvider(binding);
  let auth: ResolvedLlmBinding["auth"];
  if (binding.secretId) {
    const secret = await context.exoharness.current.conversation.getSecret(
      binding.secretId,
    );
    if (!secret) {
      throw new Error(`model secret does not exist for ${name}`);
    }
    if (secret.type === "key") {
      if (provider === "openai-chatgpt") {
        throw new Error(
          `model provider ${provider} requires an OAuth credential`,
        );
      }
      auth =
        provider === "anthropic"
          ? { headers: { "x-api-key": secret.value } }
          : {
              authorization: `Bearer ${secret.value}`,
              headers: {},
            };
    } else {
      if (!secret.provider) {
        throw new Error("OAuth credential has no provider; log in again");
      }
      if (!secret.accessToken) {
        throw new Error(
          `OAuth credential for provider ${secret.provider} is logged out`,
        );
      }
      if (secret.provider !== provider) {
        throw new Error(
          `model provider ${provider} cannot use an OAuth credential for ${secret.provider}`,
        );
      }
      if (provider !== "openai-chatgpt") {
        throw new Error(
          `OAuth request authentication is not implemented for ${provider}`,
        );
      }
      if (!secret.accountId) {
        throw new Error(
          "OpenAI ChatGPT credential has no account id; log in again",
        );
      }
      auth = {
        authorization: `Bearer ${secret.accessToken}`,
        headers: {
          "chatgpt-account-id": secret.accountId,
          "OpenAI-Beta": "responses=experimental",
        },
      };
    }
  }
  return {
    name,
    model: binding.model,
    provider,
    auth,
    baseUrl: binding.baseUrl ?? null,
  };
}

export function apiKeyFromModelBinding(
  binding: ResolvedLlmBinding,
): string | undefined {
  const headerKey = Object.entries(binding.auth?.headers ?? {}).find(
    ([name]) => name.toLowerCase() === "x-api-key",
  )?.[1];
  if (headerKey) {
    return headerKey;
  }
  return binding.auth?.authorization?.replace(/^Bearer\s+/i, "");
}

export function markFirstTextDelta(state: TextDeltaTraceState): number | null {
  if (state.sawTextDelta) {
    return null;
  }
  state.sawTextDelta = true;
  state.ttftMs = Date.now() - state.startedAt;
  return state.ttftMs;
}

export function sandboxCwd(context: TurnContext): string {
  return context.conversationConfig.mounts[0]?.mountPath ?? "/";
}

export function mountSummary(context: TurnContext): string {
  if (context.conversationConfig.mounts.length === 0) {
    return "No host filesystem mounts are configured. The sandbox only exposes its container filesystem.";
  }
  return context.conversationConfig.mounts
    .map(
      (mount) =>
        `${mount.mountPath} -> ${mount.hostPath} (${mount.mode}${mount.internal ? ", internal" : ""})`,
    )
    .join("\n");
}

export function instructionsText(messages: Message[]): string | null {
  const text = messages.map(messageText).filter(Boolean).join("\n\n");
  return text || null;
}

export async function appendEvents(
  context: TurnContext,
  events: EventData[],
  options: { defaultToolName?: string } = {},
): Promise<void> {
  if (events.length === 0) {
    return;
  }
  await context.exoharness.current.turn.addEvents(events);
  if (!context.streaming) {
    return;
  }

  for (const event of events) {
    if (event.type === "tool_requested") {
      await context.stream.toolCall({
        toolCallId: String(event.tool_call_id),
        toolName: String(
          (event.request as { function_name?: unknown } | undefined)
            ?.function_name ??
            options.defaultToolName ??
            "tool",
        ),
        arguments: asJsonObject(
          (event.request as { arguments?: unknown } | undefined)?.arguments,
        ),
      });
    } else if (event.type === "tool_result") {
      await context.stream.toolResult({
        toolCallId: String(event.tool_call_id),
        result: toJsonValue(event.result ?? null),
      });
    }
  }
}

export async function materializePriorConversationMessages(
  context: TurnContext,
): Promise<Message[]> {
  const currentTurnId = context.exoharness.current.turn.record.id;
  const result = await context.exoharness.current.conversation.getEvents({
    direction: "asc",
    types: ["messages", "tool_requested", "tool_result"],
  });
  return materializeEventsToMessages(
    result.events.filter((event) => event.turnId !== currentTurnId),
  );
}

export async function traceExoharnessToolCall(
  context: TurnContext,
  turnParent: TraceParent,
  toolCall: PendingToolCall,
  purpose: string,
): Promise<JsonValue> {
  return tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        const result = await context.executeTool(toolCall.request);
        span.log({ output: result });
        return result;
      } catch (error) {
        span.log({ error: errorMessage(error) });
        throw error;
      }
    },
    {
      name: toolCall.request.functionName,
      type: "tool",
      spanAttributes: { purpose },
      event: {
        input: toolCall.request,
        metadata: turnMetadata(context),
      },
    },
  );
}

export async function traceObservedToolCall(
  context: TurnContext,
  turnParent: TraceParent,
  toolCall: PendingToolCall,
  result: JsonValue,
  purpose: string,
): Promise<void> {
  await tracedUnderParent(
    turnParent,
    (span) => {
      span.log({ output: result });
    },
    {
      name: toolCall.request.functionName,
      type: "tool",
      spanAttributes: { purpose },
      event: {
        input: toolCall.request,
        metadata: turnMetadata(context),
      },
    },
  );
}

export async function appendAndTraceObservedToolEvents(
  context: TurnContext,
  turnParent: TraceParent,
  events: EventData[],
  activeToolCalls: Map<string, PendingToolCall>,
  purpose: string,
): Promise<void> {
  if (events.length === 0) {
    return;
  }
  await appendEvents(context, events);
  for (const event of events) {
    if (isToolRequestedProjection(event)) {
      activeToolCalls.set(event.tool_call_id, {
        toolCallId: event.tool_call_id,
        request: {
          functionName: event.request.function_name,
          arguments: event.request.arguments,
        },
      });
    } else if (isToolResultProjection(event)) {
      const toolCall = activeToolCalls.get(event.tool_call_id);
      if (toolCall) {
        await traceObservedToolCall(
          context,
          turnParent,
          toolCall,
          toJsonValue(event.result ?? null),
          purpose,
        );
      }
      activeToolCalls.delete(event.tool_call_id);
    }
  }
}

export function shellToolSucceeded(result: JsonValue): boolean {
  const exitCode = asRecord(result).exit_code;
  return typeof exitCode === "number" ? exitCode === 0 : true;
}

export function shellToolResultText(result: JsonValue): string {
  const record = asRecord(result);
  if (
    typeof record.stdout === "string" ||
    typeof record.stderr === "string" ||
    typeof record.exit_code === "number"
  ) {
    return [
      `exit_code: ${record.exit_code ?? "unknown"}`,
      `stdout:\n${record.stdout ?? ""}`,
      `stderr:\n${record.stderr ?? ""}`,
    ].join("\n");
  }
  return stringifyValue(result);
}

export function pickEnv(
  predicate: (key: string) => boolean,
): Record<string, string> {
  return pickEnvFrom(process.env, predicate);
}

export function pickEnvFrom(
  env: Record<string, string | undefined>,
  predicate: (key: string) => boolean,
): Record<string, string> {
  const selected: Record<string, string> = {};
  for (const [key, value] of Object.entries(env)) {
    if (value && predicate(key)) {
      selected[key] = value;
    }
  }
  return selected;
}

export function objectArgs(value: Record<string, unknown>): JsonObject {
  return asJsonObject(toJsonValue(value));
}

export function asJsonObject(value: unknown): JsonObject {
  return isRecord(value) ? (toJsonValue(value) as JsonObject) : {};
}

export function asRecord(value: unknown): Record<string, unknown> {
  return isRecord(value) ? value : {};
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

export function stringOrNull(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

export function numberField(value: unknown): number | undefined {
  return typeof value === "number" ? value : undefined;
}

function isToolRequestedProjection(data: EventData): data is EventData & {
  type: "tool_requested";
  tool_call_id: string;
  request: { function_name: string; arguments: JsonObject };
} {
  return (
    data.type === "tool_requested" &&
    typeof data.tool_call_id === "string" &&
    isRecord(data.request) &&
    typeof data.request.function_name === "string" &&
    isRecord(data.request.arguments)
  );
}

function isToolResultProjection(data: EventData): data is EventData & {
  type: "tool_result";
  tool_call_id: string;
  result: JsonValue;
} {
  return data.type === "tool_result" && typeof data.tool_call_id === "string";
}
