import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { afterEach, describe, expect, it } from "vitest";

import {
  parsePiWorkerEvent,
  type PiWorkerEvent,
  type PiWorkerRequest,
  type PiWorkerRunResult,
} from "./protocol";

const workerPath = fileURLToPath(
  new URL("./sandbox-worker.ts", import.meta.url),
);
const temporaryDirectories: string[] = [];

interface MockServer {
  baseUrl: string;
  requests: unknown[];
  close(): Promise<void>;
}

interface WorkerRun {
  events: PiWorkerEvent[];
  result: PiWorkerRunResult;
  stderr: string;
}

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

describe("Pi sandbox worker", () => {
  it("streams a model response, reports usage, and ignores ambient Pi resources", async () => {
    const cwd = temporaryDirectory("exo-pi-worker-");
    const marker = join(cwd, "ambient-extension-loaded");
    const agentDir = join(cwd, "pi-agent");
    await mkdir(join(agentDir, "extensions"), { recursive: true });
    await mkdir(join(cwd, ".pi"), { recursive: true });
    writeFileSync(
      join(agentDir, "extensions", "untrusted.ts"),
      [
        'import { writeFileSync } from "node:fs";',
        `writeFileSync(${JSON.stringify(marker)}, "loaded");`,
        "export default function () {}",
      ].join("\n"),
    );
    const ambientInstruction = "AMBIENT_PI_INSTRUCTION_MUST_NOT_LOAD";
    writeFileSync(join(cwd, ".pi", "settings.json"), "{}\n");
    writeFileSync(join(cwd, "AGENTS.md"), `${ambientInstruction}\n`);
    writeFileSync(
      join(cwd, ".pi", "APPEND_SYSTEM.md"),
      `${ambientInstruction}\n`,
    );

    const server = await startMockServer((_request, response) => {
      sendTextResponse(response, "mock ok", { input: 7, output: 2 });
    });
    try {
      const run = await runWorker({
        requestId: "request-text",
        prompt: "Say mock ok.",
        systemPrompt: "isolated Exoharness system prompt",
        model: "anthropic/claude-sonnet-4-6",
        baseUrl: server.baseUrl,
        cwd,
        maxOutputTokens: 123,
      });

      expect(run.events.map((event) => event.type)).toEqual([
        "run_started",
        "delta",
        "message",
        "completed",
      ]);
      expect(
        run.events.every((event) => event.requestId === "request-text"),
      ).toBe(true);
      expect(run.result).toMatchObject({
        status: "finished",
        finalText: "mock ok",
        model: "claude-sonnet-4-6",
        provider: "anthropic",
        usage: {
          input: 7,
          output: 2,
          totalTokens: 9,
        },
      });
      expect(run.result.durationMs).toBeGreaterThanOrEqual(0);
      const assistantMessage = run.events.find(
        (event) => event.type === "message",
      );
      expect(assistantMessage).toMatchObject({
        type: "message",
        durationMs: expect.any(Number),
        ttftMs: expect.any(Number),
      });
      const requests = JSON.stringify(server.requests);
      expect(requests).toContain("isolated Exoharness system prompt");
      expect(requests).toContain('"max_tokens":123');
      expect(requests).not.toContain(ambientInstruction);
      expect(existsSync(marker)).toBe(false);
      expect(run.stderr).toBe("");
    } finally {
      await server.close();
    }
  }, 30_000);

  it("executes Pi tools and preserves request-scoped tool events", async () => {
    const cwd = temporaryDirectory("exo-pi-worker-tool-");
    const file = join(cwd, "tool-marker.txt");
    writeFileSync(file, "tool-marker-value\n");
    let requestCount = 0;
    const server = await startMockServer((request, response) => {
      requestCount += 1;
      if (requestCount === 1) {
        sendToolCallResponse(response, "tool-call-1", "read", { path: file });
        return;
      }
      expect(JSON.stringify(request)).toContain("tool-marker-value");
      sendTextResponse(response, "read tool-marker-value", {
        input: 6,
        output: 3,
      });
    });

    try {
      const run = await runWorker({
        requestId: "request-tool",
        prompt: "Read the marker file and report its contents.",
        systemPrompt: "Use the available tools.",
        model: "anthropic/claude-sonnet-4-6",
        baseUrl: server.baseUrl,
        cwd,
      });

      expect(requestCount).toBe(2);
      expect(run.events.map((event) => event.type)).toEqual([
        "run_started",
        "message",
        "tool_start",
        "tool_end",
        "message",
        "delta",
        "message",
        "completed",
      ]);
      const toolStart = run.events.find((event) => event.type === "tool_start");
      const toolEnd = run.events.find((event) => event.type === "tool_end");
      expect(toolStart).toMatchObject({
        type: "tool_start",
        requestId: "request-tool",
        callId: "tool-call-1",
        name: "read",
        args: { path: file },
      });
      expect(toolEnd).toMatchObject({
        type: "tool_end",
        requestId: "request-tool",
        callId: "tool-call-1",
        name: "read",
        isError: false,
      });
      expect(JSON.stringify(toolEnd)).toContain("tool-marker-value");
      expect(run.result).toMatchObject({
        status: "finished",
        finalText: "read tool-marker-value",
        usage: {
          input: 11,
          output: 4,
          totalTokens: 15,
        },
      });
    } finally {
      await server.close();
    }
  }, 30_000);

  it("does not expose the provider credential to Pi's bash tool", async () => {
    const cwd = temporaryDirectory("exo-pi-worker-credential-");
    let requestCount = 0;
    const server = await startMockServer((request, response) => {
      requestCount += 1;
      if (requestCount === 1) {
        sendToolCallResponse(response, "tool-call-env", "bash", {
          command:
            'if [ -z "${EXO_PI_API_KEY+x}" ]; then printf credential-hidden; else printf credential-exposed; fi',
        });
        return;
      }
      sendTextResponse(response, "credential-hidden", { input: 5, output: 2 });
    });

    try {
      const run = await runWorker({
        requestId: "request-credential",
        prompt: "Check whether the provider credential is in the environment.",
        systemPrompt: "Use the available tools.",
        model: "anthropic/claude-sonnet-4-6",
        baseUrl: server.baseUrl,
        cwd,
      });

      expect(requestCount).toBe(2);
      const followUpRequest = JSON.stringify(server.requests[1]);
      expect(followUpRequest).toContain(
        '"content":"credential-hidden","is_error":false',
      );
      expect(followUpRequest).not.toContain(
        '"content":"credential-exposed","is_error":false',
      );
      expect(run.result).toMatchObject({
        status: "finished",
        finalText: "credential-hidden",
      });
    } finally {
      await server.close();
    }
  }, 30_000);

  it("keeps sequential warm-worker events scoped to their requests", async () => {
    const cwd = temporaryDirectory("exo-pi-worker-sequence-");
    const server = await startMockServer((request, response) => {
      const body = JSON.stringify(request);
      sendTextResponse(
        response,
        body.includes("second prompt") ? "second response" : "first response",
        { input: 4, output: 2 },
      );
    });

    try {
      const common = {
        systemPrompt: "isolated system prompt",
        model: "anthropic/claude-sonnet-4-6",
        baseUrl: server.baseUrl,
        cwd,
      };
      const run = await runWorkerSequence([
        { ...common, requestId: "sequence-1", prompt: "first prompt" },
        { ...common, requestId: "sequence-2", prompt: "second prompt" },
      ]);

      expect(run.results.map((result) => result.finalText)).toEqual([
        "first response",
        "second response",
      ]);
      expect(
        run.events
          .filter((event) => event.type === "completed")
          .map((event) => event.requestId),
      ).toEqual(["sequence-1", "sequence-2"]);
      const firstCompleted = run.events.findIndex(
        (event) =>
          event.type === "completed" && event.requestId === "sequence-1",
      );
      const secondStarted = run.events.findIndex(
        (event) =>
          event.type === "run_started" && event.requestId === "sequence-2",
      );
      expect(secondStarted).toBeGreaterThan(firstCompleted);
    } finally {
      await server.close();
    }
  }, 30_000);

  it("stops gracefully at the configured tool round-trip budget", async () => {
    const cwd = temporaryDirectory("exo-pi-worker-budget-");
    const file = join(cwd, "tool-marker.txt");
    writeFileSync(file, "tool-marker-value\n");
    let requestCount = 0;
    const server = await startMockServer((_request, response) => {
      requestCount += 1;
      sendToolCallResponse(response, "tool-call-budget", "read", {
        path: file,
      });
    });

    try {
      const run = await runWorker({
        requestId: "request-budget",
        prompt: "Read the marker file.",
        systemPrompt: "Use the available tools.",
        model: "anthropic/claude-sonnet-4-6",
        baseUrl: server.baseUrl,
        cwd,
        maxToolRoundTrips: 0,
      });
      expect(run.result).toMatchObject({
        status: "finished",
        finalText: "",
      });
      expect(requestCount).toBe(1);
    } finally {
      await server.close();
    }
  }, 30_000);
});

async function runWorker(request: PiWorkerRequest): Promise<WorkerRun> {
  const run = await runWorkerSequence([request]);
  const result = run.results[0];
  if (!result) {
    throw new Error("Pi worker sequence returned no result");
  }
  return { events: run.events, result, stderr: run.stderr };
}

async function runWorkerSequence(requests: PiWorkerRequest[]): Promise<{
  events: PiWorkerEvent[];
  results: PiWorkerRunResult[];
  stderr: string;
}> {
  const firstRequest = requests[0];
  if (!firstRequest) {
    throw new Error("Pi worker sequence requires at least one request");
  }
  const child = spawn(process.execPath, ["--import", "tsx", workerPath], {
    cwd: process.cwd(),
    env: {
      ...process.env,
      EXO_PI_AGENT_DIR: join(firstRequest.cwd, "pi-agent"),
      EXO_PI_API_KEY: "test-key",
      PI_OFFLINE: "1",
    },
    stdio: ["pipe", "pipe", "pipe"],
  });
  const events: PiWorkerEvent[] = [];
  const results: PiWorkerRunResult[] = [];
  const terminalWaiters = new Map<
    string,
    {
      resolve(event: PiWorkerEvent): void;
      reject(error: Error): void;
    }
  >();
  let stderr = "";
  let stdout = "";
  child.stderr.on("data", (chunk: Buffer) => {
    stderr += chunk.toString();
  });
  child.stdout.on("data", (chunk: Buffer) => {
    stdout += chunk.toString();
    while (true) {
      const newline = stdout.indexOf("\n");
      if (newline < 0) {
        return;
      }
      const line = stdout.slice(0, newline).trim();
      stdout = stdout.slice(newline + 1);
      if (!line) {
        continue;
      }
      try {
        const event = parsePiWorkerEvent(line);
        events.push(event);
        if (event.type === "completed" || event.type === "error") {
          terminalWaiters.get(event.requestId)?.resolve(event);
        }
      } catch (error) {
        const invalid = new Error(`invalid worker output: ${line}`, {
          cause: error,
        });
        for (const waiter of terminalWaiters.values()) {
          waiter.reject(invalid);
        }
      }
    }
  });
  const rejectAll = (error: Error) => {
    for (const waiter of terminalWaiters.values()) {
      waiter.reject(error);
    }
  };
  child.on("error", rejectAll);
  child.on("exit", (code) => {
    if (code !== 0) {
      rejectAll(new Error(`Pi worker exited with ${code}; stderr:\n${stderr}`));
    }
  });

  try {
    for (const request of requests) {
      const terminal = new Promise<PiWorkerEvent>((resolve, reject) => {
        terminalWaiters.set(request.requestId, { resolve, reject });
      });
      child.stdin.write(`${JSON.stringify(request)}\n`);
      const terminalEvent = await withWorkerTimeout(terminal, () => stderr);
      terminalWaiters.delete(request.requestId);
      if (terminalEvent.type === "error") {
        throw new Error(terminalEvent.message);
      }
      if (terminalEvent.type !== "completed") {
        throw new Error(`unexpected terminal event: ${terminalEvent.type}`);
      }
      results.push(terminalEvent.result);
    }
    child.stdin.end();
    await once(child, "exit");
    return { events, results, stderr: stderr.trim() };
  } finally {
    if (child.exitCode === null) {
      child.kill("SIGTERM");
    }
  }
}

async function withWorkerTimeout<T>(
  value: Promise<T>,
  stderr: () => string,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_resolve, reject) => {
    timer = setTimeout(() => {
      reject(new Error(`Pi worker timed out; stderr:\n${stderr()}`));
    }, 25_000);
    timer.unref?.();
  });
  try {
    return await Promise.race([value, timeout]);
  } finally {
    if (timer) {
      clearTimeout(timer);
    }
  }
}

async function startMockServer(
  handler: (request: unknown, response: ServerResponse) => void,
): Promise<MockServer> {
  const requests: unknown[] = [];
  const server = http.createServer(
    async (request: IncomingMessage, response: ServerResponse) => {
      try {
        let body = "";
        for await (const chunk of request) {
          body += chunk.toString();
        }
        const parsed = JSON.parse(body) as unknown;
        requests.push(parsed);
        handler(parsed, response);
      } catch (error) {
        response.writeHead(500, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: String(error) }));
      }
    },
  );
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("mock server did not expose a TCP port");
  }
  return {
    baseUrl: `http://127.0.0.1:${address.port}`,
    requests,
    close: async () => {
      server.close();
      await once(server, "close");
    },
  };
}

function sendTextResponse(
  response: ServerResponse,
  text: string,
  usage: { input: number; output: number },
): void {
  sendAnthropicEvents(response, [
    [
      "message_start",
      {
        type: "message_start",
        message: {
          id: `message-${Date.now()}`,
          type: "message",
          role: "assistant",
          content: [],
          model: "claude-sonnet-4-6",
          stop_reason: null,
          stop_sequence: null,
          usage: { input_tokens: usage.input, output_tokens: 0 },
        },
      },
    ],
    [
      "content_block_start",
      {
        type: "content_block_start",
        index: 0,
        content_block: { type: "text", text: "" },
      },
    ],
    [
      "content_block_delta",
      {
        type: "content_block_delta",
        index: 0,
        delta: { type: "text_delta", text },
      },
    ],
    ["content_block_stop", { type: "content_block_stop", index: 0 }],
    [
      "message_delta",
      {
        type: "message_delta",
        delta: { stop_reason: "end_turn", stop_sequence: null },
        usage: { output_tokens: usage.output },
      },
    ],
    ["message_stop", { type: "message_stop" }],
  ]);
}

function sendToolCallResponse(
  response: ServerResponse,
  callId: string,
  name: string,
  args: Record<string, unknown>,
): void {
  sendAnthropicEvents(response, [
    [
      "message_start",
      {
        type: "message_start",
        message: {
          id: `message-${Date.now()}`,
          type: "message",
          role: "assistant",
          content: [],
          model: "claude-sonnet-4-6",
          stop_reason: null,
          stop_sequence: null,
          usage: { input_tokens: 5, output_tokens: 0 },
        },
      },
    ],
    [
      "content_block_start",
      {
        type: "content_block_start",
        index: 0,
        content_block: { type: "tool_use", id: callId, name, input: {} },
      },
    ],
    [
      "content_block_delta",
      {
        type: "content_block_delta",
        index: 0,
        delta: {
          type: "input_json_delta",
          partial_json: JSON.stringify(args),
        },
      },
    ],
    ["content_block_stop", { type: "content_block_stop", index: 0 }],
    [
      "message_delta",
      {
        type: "message_delta",
        delta: { stop_reason: "tool_use", stop_sequence: null },
        usage: { output_tokens: 1 },
      },
    ],
    ["message_stop", { type: "message_stop" }],
  ]);
}

function sendAnthropicEvents(
  response: ServerResponse,
  events: Array<[string, Record<string, unknown>]>,
): void {
  response.writeHead(200, { "content-type": "text/event-stream" });
  for (const [event, data] of events) {
    response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  }
  response.end();
}

function temporaryDirectory(prefix: string): string {
  const directory = mkdtempSync(join(tmpdir(), prefix));
  temporaryDirectories.push(directory);
  return directory;
}
