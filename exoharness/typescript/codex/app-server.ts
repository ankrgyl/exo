import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

import type { JsonValue, SandboxProcess } from "@exo/harness";

export type CodexDirection = "client_to_server" | "server_to_client";

export interface CodexProtocolLogEntry {
  sequence: number;
  direction: CodexDirection;
  message: JsonValue;
}

export type CodexProtocolLogger = (
  entry: CodexProtocolLogEntry,
) => Promise<void> | void;

export interface CodexServerRequest {
  method: string;
  params: JsonValue | null;
}

export type CodexServerRequestHandler = (
  request: CodexServerRequest,
) => Promise<JsonValue | undefined> | JsonValue | undefined;

export interface CodexAppServerOptions {
  executable?: string;
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  onProtocolMessage?: CodexProtocolLogger;
  onServerRequest?: CodexServerRequestHandler;
}

export interface CodexAppServerSandboxOptions extends Pick<
  CodexAppServerOptions,
  "onProtocolMessage" | "onServerRequest"
> {
  process: SandboxProcess;
}

export interface CodexNotification {
  method: string;
  params: JsonValue | null;
}

interface CodexAppServerTransport {
  stdout: AsyncIterable<string>;
  stderr: AsyncIterable<string>;
  write(data: string): Promise<void>;
  close(): void | Promise<void>;
}

interface PendingRequest {
  method: string;
  resolve: (value: JsonValue) => void;
  reject: (error: Error) => void;
}

type ProtocolMessage = Record<string, unknown>;
type ProtocolParseResult =
  | { type: "message"; message: ProtocolMessage }
  | { type: "incomplete" };

export class CodexAppServer {
  private readonly transport: CodexAppServerTransport;
  private readonly notifications = new AsyncQueue<CodexNotification>();
  private readonly pending = new Map<number, PendingRequest>();
  private readonly onProtocolMessage?: CodexProtocolLogger;
  private readonly onServerRequest?: CodexServerRequestHandler;
  private closing = false;
  private finished = false;
  private nextId = 1;
  private sequence = 0;
  private stderr = "";

  private constructor(
    transport: CodexAppServerTransport,
    onProtocolMessage?: CodexProtocolLogger,
    onServerRequest?: CodexServerRequestHandler,
  ) {
    this.transport = transport;
    this.onProtocolMessage = onProtocolMessage;
    this.onServerRequest = onServerRequest;
    void this.readStderrLoop();
    void this.readLoop();
  }

  static async start(
    options: CodexAppServerOptions = {},
  ): Promise<CodexAppServer> {
    const executable = options.executable ?? process.env.CODEX_BIN ?? "codex";
    const child = spawn(executable, ["app-server", "--listen", "stdio://"], {
      cwd: options.cwd,
      env: { ...process.env, ...options.env },
      stdio: "pipe",
    }) as ChildProcessWithoutNullStreams;
    const server = new CodexAppServer(
      {
        stdout: nodeStreamChunks(child.stdout),
        stderr: nodeStreamChunks(child.stderr),
        write: (data) => writeNodeStdin(child, data),
        close: () => {
          child.kill();
        },
      },
      options.onProtocolMessage,
      options.onServerRequest,
    );
    child.once("error", (error) => server.fail(error));
    child.once("exit", (code, signal) => {
      if ((code === 0 && signal === null) || server.closing) {
        server.finish();
      } else {
        server.fail(
          new Error(
            `codex app-server exited with ${signal ? `signal ${signal}` : `code ${code ?? 1}`}${server.stderrSuffix()}`,
          ),
        );
      }
    });
    await server.initialize();
    return server;
  }

  static async startInSandbox(
    options: CodexAppServerSandboxOptions,
  ): Promise<CodexAppServer> {
    const server = CodexAppServer.createSandboxServer(options);
    await server.initialize();
    return server;
  }

  static async attachToSandbox(
    options: CodexAppServerSandboxOptions,
  ): Promise<CodexAppServer> {
    return CodexAppServer.createSandboxServer(options);
  }

  private static createSandboxServer(
    options: CodexAppServerSandboxOptions,
  ): CodexAppServer {
    const server = new CodexAppServer(
      {
        stdout: readableStreamChunks(options.process.stdout),
        stderr: readableStreamChunks(options.process.stderr),
        write: (data) => options.process.writeStdin(data),
        close: () => options.process.close(),
      },
      options.onProtocolMessage,
      options.onServerRequest,
    );
    void options.process
      .wait()
      .then((exitCode) => {
        if (exitCode === 0 || exitCode === null || server.closing) {
          server.finish();
        } else {
          server.fail(
            new Error(
              `codex app-server exited with code ${exitCode}${server.stderrSuffix()}`,
            ),
          );
        }
      })
      .catch((error: unknown) => {
        server.fail(error instanceof Error ? error : new Error(String(error)));
      });
    return server;
  }

  private async initialize(): Promise<void> {
    await this.request("initialize", {
      clientInfo: {
        name: "exo",
        title: "Exo",
        version: "0.1.0",
      },
      capabilities: {
        experimentalApi: true,
      },
    });
    await this.notify("initialized");
  }

  async request<T extends JsonValue = JsonValue>(
    method: string,
    params: JsonValue = {},
  ): Promise<T> {
    const id = this.nextId;
    this.nextId += 1;
    const message = { method, id, params };
    const response = new Promise<JsonValue>((resolve, reject) => {
      this.pending.set(id, { method, resolve, reject });
    });
    await this.write(message);
    return (await response) as T;
  }

  async notify(method: string, params?: JsonValue): Promise<void> {
    const message = params === undefined ? { method } : { method, params };
    await this.write(message);
  }

  async *events(): AsyncGenerator<CodexNotification> {
    while (true) {
      const item = await this.notifications.next();
      if (item.done) {
        return;
      }
      yield item.value;
    }
  }

  close(): void {
    this.closing = true;
    void this.transport.close();
  }

  private async readLoop(): Promise<void> {
    try {
      let pendingLine = "";
      for await (const line of linesFromChunks(this.transport.stdout)) {
        const candidate = `${pendingLine}${line}`;
        const parsed = tryParseProtocolMessage(candidate);
        if (parsed.type === "incomplete") {
          pendingLine = candidate;
          continue;
        }
        pendingLine = "";
        await this.handleProtocolMessage(parsed.message);
      }
      if (pendingLine.trim().length > 0) {
        await this.handleProtocolMessage(parseProtocolMessage(pendingLine));
      }
      this.finish();
    } catch (error) {
      this.fail(error instanceof Error ? error : new Error(String(error)));
    }
  }

  private async readStderrLoop(): Promise<void> {
    try {
      for await (const chunk of this.transport.stderr) {
        this.stderr += chunk;
      }
    } catch (error) {
      this.stderr += `\nfailed to read stderr: ${error instanceof Error ? error.message : String(error)}`;
    }
  }

  private handleResponse(message: ProtocolMessage): void {
    const id = Number(message.id);
    const pending = this.pending.get(id);
    if (!pending) {
      return;
    }
    this.pending.delete(id);
    if (
      "error" in message &&
      message.error !== null &&
      message.error !== undefined
    ) {
      pending.reject(new Error(protocolErrorMessage(message.error)));
    } else {
      pending.resolve(toJsonOrNull(message.result));
    }
  }

  private async handleProtocolMessage(message: ProtocolMessage): Promise<void> {
    if (this.isEchoedClientRequest(message)) {
      return;
    }
    await this.record("server_to_client", message);
    if (isResponse(message)) {
      this.handleResponse(message);
    } else if (isServerRequest(message)) {
      const request = {
        method: message.method,
        params: toJsonOrNull(message.params),
      };
      this.notifications.push(request);
      await this.write({
        id: message.id,
        result: await this.serverRequestResult(request),
      });
    } else if (isNotification(message)) {
      this.notifications.push({
        method: message.method,
        params: toJsonOrNull(message.params),
      });
    }
  }

  private isEchoedClientRequest(message: ProtocolMessage): boolean {
    if (!("id" in message) || typeof message.method !== "string") {
      return false;
    }
    const pending = this.pending.get(Number(message.id));
    return pending?.method === message.method;
  }

  private async serverRequestResult(
    request: CodexServerRequest,
  ): Promise<JsonValue | undefined> {
    const handled = await this.onServerRequest?.(request);
    return handled === undefined
      ? defaultServerRequestResult(request.method)
      : handled;
  }

  private async write(message: ProtocolMessage): Promise<void> {
    await this.record("client_to_server", message);
    await this.transport.write(`${JSON.stringify(message)}\n`);
  }

  private async record(
    direction: CodexDirection,
    message: ProtocolMessage,
  ): Promise<void> {
    const onProtocolMessage = this.onProtocolMessage;
    if (!onProtocolMessage) {
      return;
    }
    this.sequence += 1;
    await onProtocolMessage({
      sequence: this.sequence,
      direction,
      message: JSON.parse(JSON.stringify(message)) as JsonValue,
    });
  }

  private fail(error: Error): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    for (const pending of this.pending.values()) {
      pending.reject(error);
    }
    this.pending.clear();
    this.notifications.fail(error);
  }

  private finish(): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    const error = new Error("codex app-server closed before responding");
    for (const pending of this.pending.values()) {
      pending.reject(error);
    }
    this.pending.clear();
    this.notifications.end();
  }

  private stderrSuffix(): string {
    const stderr = this.stderr.trim();
    return stderr ? `\nstderr:\n${stderr}` : "";
  }
}

async function* nodeStreamChunks(
  stream: NodeJS.ReadableStream,
): AsyncGenerator<string> {
  for await (const chunk of stream as AsyncIterable<Buffer | string>) {
    yield String(chunk);
  }
}

async function* readableStreamChunks(
  stream: ReadableStream<string>,
): AsyncGenerator<string> {
  const reader = stream.getReader();
  try {
    while (true) {
      const result = await reader.read();
      if (result.done) {
        return;
      }
      yield result.value;
    }
  } finally {
    reader.releaseLock();
  }
}

async function* linesFromChunks(
  chunks: AsyncIterable<string>,
): AsyncGenerator<string> {
  let buffered = "";
  for await (const chunk of chunks) {
    buffered += chunk;
    while (true) {
      const newline = buffered.indexOf("\n");
      if (newline < 0) {
        break;
      }
      const line = buffered.slice(0, newline).replace(/\r$/, "");
      buffered = buffered.slice(newline + 1);
      if (line.length > 0) {
        yield line;
      }
    }
  }
  if (buffered.trim().length > 0) {
    yield buffered;
  }
}

function writeNodeStdin(
  child: ChildProcessWithoutNullStreams,
  data: string,
): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    child.stdin.write(data, (error) => {
      if (error) {
        reject(error);
      } else {
        resolve();
      }
    });
  });
}

function parseProtocolMessage(line: string): ProtocolMessage {
  const parsed = JSON.parse(line) as unknown;
  if (!isRecord(parsed)) {
    throw new Error(`invalid codex app-server message: ${line}`);
  }
  return parsed;
}

function tryParseProtocolMessage(line: string): ProtocolParseResult {
  try {
    return { type: "message", message: parseProtocolMessage(line) };
  } catch (error) {
    if (isIncompleteJsonParseError(error)) {
      return { type: "incomplete" };
    }
    throw error;
  }
}

function isIncompleteJsonParseError(error: unknown): boolean {
  if (!(error instanceof SyntaxError)) {
    return false;
  }
  return (
    error.message.includes("Unterminated string") ||
    error.message.includes("Unexpected end of JSON input")
  );
}

function isResponse(message: ProtocolMessage): boolean {
  return "id" in message && ("result" in message || "error" in message);
}

function isServerRequest(
  message: ProtocolMessage,
): message is ProtocolMessage & { id: string | number; method: string } {
  return "id" in message && typeof message.method === "string";
}

function defaultServerRequestResult(method: string): JsonValue {
  switch (method) {
    case "item/commandExecution/requestApproval":
    case "item/fileChange/requestApproval":
      return { decision: "decline" };
    case "item/permissions/requestApproval":
      return { scope: "turn", permissions: {} };
    case "mcpServer/elicitation/request":
      return { action: "decline", content: null };
    case "item/tool/requestUserInput":
    case "tool/requestUserInput":
      return { action: "cancel", answers: {} };
    default:
      return null;
  }
}

function isNotification(
  message: ProtocolMessage,
): message is ProtocolMessage & { method: string } {
  return !("id" in message) && typeof message.method === "string";
}

function toJsonOrNull(value: unknown): JsonValue | null {
  if (value === undefined) {
    return null;
  }
  return JSON.parse(JSON.stringify(value)) as JsonValue;
}

function protocolErrorMessage(error: unknown): string {
  if (isRecord(error) && typeof error.message === "string") {
    return error.message;
  }
  return JSON.stringify(error);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

class AsyncQueue<T> {
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
      const waiter = this.waiters.shift();
      waiter?.resolve({ done: true, value: undefined });
    }
  }

  fail(error: Error): void {
    this.error = error;
    while (this.waiters.length > 0) {
      const waiter = this.waiters.shift();
      waiter?.reject(error);
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
