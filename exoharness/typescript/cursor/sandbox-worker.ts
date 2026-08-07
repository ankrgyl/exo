import readline from "node:readline/promises";

import {
  CursorSdkSession,
  cursorSdkApiKey,
  cursorSdkErrorMessage,
} from "./sdk";
import {
  toCursorJson,
  type CursorWorkerEvent,
  type CursorWorkerRequest,
} from "./protocol";

async function main(): Promise<void> {
  const session = new CursorSdkSession();
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  try {
    for await (const line of rl) {
      if (line.trim()) {
        await handleRequest(session, parseRequest(line));
      }
    }
  } finally {
    rl.close();
    await session.close();
  }
}

async function handleRequest(
  session: CursorSdkSession,
  request: CursorWorkerRequest,
): Promise<void> {
  await session
    .run(request.prompt, {
      apiKey: cursorSdkApiKey(),
      cwd: request.cwd,
      model: request.model,
      name: request.name,
      onRunStarted: async ({ agentId, runId }) => {
        writeEvent({ type: "run_started", agentId, runId });
      },
      onDelta: async (update) => {
        writeEvent({ type: "delta", update: toCursorJson(update) });
      },
      onMessage: async (message) => {
        writeEvent({ type: "message", message: toCursorJson(message) });
      },
    })
    .then((run) => {
      writeEvent({
        type: "completed",
        agentId: run.agentId,
        runId: run.runId,
        result: {
          id: run.result.id,
          status: run.result.status,
          result: run.result.result,
          model: toCursorJson(run.result.model),
          durationMs: run.result.durationMs,
          git: toCursorJson(run.result.git),
        },
      });
    })
    .catch((error: unknown) => {
      writeEvent({
        type: "error",
        message: cursorSdkErrorMessage(error),
        error: toCursorJson(error),
      });
    });
}

function parseRequest(line: string): CursorWorkerRequest {
  const parsed = JSON.parse(line) as unknown;
  if (!isRecord(parsed)) {
    throw new Error("cursor sandbox worker request must be a JSON object");
  }
  if (typeof parsed.prompt !== "string") {
    throw new Error("cursor sandbox worker request requires prompt");
  }
  if (typeof parsed.model !== "string") {
    throw new Error("cursor sandbox worker request requires model");
  }
  if (typeof parsed.cwd !== "string") {
    throw new Error("cursor sandbox worker request requires cwd");
  }
  return {
    prompt: parsed.prompt,
    model: parsed.model,
    cwd: parsed.cwd,
    name: typeof parsed.name === "string" ? parsed.name : undefined,
  };
}

function writeEvent(event: CursorWorkerEvent): void {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

void main().catch((error: unknown) => {
  writeEvent({
    type: "error",
    message: cursorSdkErrorMessage(error),
    error: toCursorJson(error),
  });
  process.exitCode = 1;
});
