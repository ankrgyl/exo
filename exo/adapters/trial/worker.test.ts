import { spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { afterEach, describe, expect, it } from "vitest";

class LineQueue {
  private readonly lines: string[] = [];
  private readonly waiters: Array<(line: string) => void> = [];

  constructor(stream: NodeJS.ReadableStream) {
    readline
      .createInterface({ input: stream, crlfDelay: Number.POSITIVE_INFINITY })
      .on("line", (line) => {
        const waiter = this.waiters.shift();
        if (waiter) {
          waiter(line);
        } else {
          this.lines.push(line);
        }
      });
  }

  async next(): Promise<string> {
    const line = this.lines.shift();
    if (line !== undefined) {
      return line;
    }
    return new Promise((resolve) => this.waiters.push(resolve));
  }
}

const children: ReturnType<typeof spawn>[] = [];
const tempDirs: string[] = [];

afterEach(() => {
  for (const child of children.splice(0)) {
    child.kill("SIGKILL");
  }
  for (const tempDir of tempDirs.splice(0)) {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
});

describe("trial adapter worker", () => {
  it("forwards cancellation and returns the snapshot response", async () => {
    const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "exo-trial-worker-"));
    tempDirs.push(tempDir);
    const socketPath = path.join(tempDir, "trial.sock");
    const stateDir = path.join(tempDir, "state");
    const worker = startWorker(socketPath, stateDir);
    await worker.events.next();

    const waiting = await connect(socketPath);
    waiting.write(
      `${JSON.stringify({
        type: "trial_run",
        request_id: "request-1",
        target: "trial-1",
        container_id: "container-1",
        instructions: "Solve it",
      })}\n`,
    );
    await worker.events.next();

    const cancelling = await connect(socketPath);
    const responses = new LineQueue(cancelling);
    cancelling.write(
      `${JSON.stringify({
        type: "trial_cancel",
        request_id: "request-1",
        target: "trial-1",
      })}\n`,
    );
    expect(JSON.parse(await worker.events.next())).toMatchObject({
      type: "control",
      target: "trial-1",
      metadata: { type: "trial_cancel", request_id: "request-1" },
    });
    const workerInput = worker.child.stdin;
    if (workerInput === null) {
      throw new Error("trial worker stdin is not piped");
    }
    workerInput.write(
      `${JSON.stringify({
        type: "send_message",
        id: "cancel-command-1",
        target: "trial-1",
        text: JSON.stringify({
          type: "trial_cancelled",
          request_id: "request-1",
          target: "trial-1",
          conversation_id: "conversation-1",
          snapshot_id: "snapshot-1",
        }),
      })}\n`,
    );
    expect(JSON.parse(await responses.next())).toEqual({
      type: "response",
      event: {
        type: "trial_cancelled",
        request_id: "request-1",
        target: "trial-1",
        conversation_id: "conversation-1",
        snapshot_id: "snapshot-1",
      },
    });
    expect(fs.readdirSync(stateDir)).toHaveLength(1);
    expect(fs.readdirSync(stateDir)[0]).toMatch(/^response-/);
    waiting.destroy();
    cancelling.end();
  });

  it("recovers a pending trial after restart and replays its response", async () => {
    const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "exo-trial-worker-"));
    tempDirs.push(tempDir);
    const socketPath = path.join(tempDir, "trial.sock");
    const stateDir = path.join(tempDir, "state");
    const request = {
      type: "trial_run",
      request_id: "request-1",
      target: "trial-1",
      container_id: "container-1",
      instructions: "Solve it",
    };

    const first = startWorker(socketPath, stateDir);
    expect(JSON.parse(await first.events.next())).toMatchObject({
      type: "connected",
      subject: socketPath,
    });
    const firstClient = await connect(socketPath);
    const firstResponses = new LineQueue(firstClient);
    firstClient.write(`${JSON.stringify(request)}\n`);
    expect(JSON.parse(await first.events.next())).toMatchObject({
      type: "message",
      target: "trial-1",
      message_id: "request-1",
      metadata: {
        request_id: "request-1",
        container_id: "container-1",
      },
    });
    const firstInput = first.child.stdin;
    if (firstInput === null) {
      throw new Error("trial worker stdin is not piped");
    }
    firstInput.write(
      `${JSON.stringify({
        type: "send_message",
        id: "started-command-1",
        target: "trial-1",
        text: JSON.stringify({
          type: "trial_started",
          request_id: "request-1",
          target: "trial-1",
          conversation_id: "conversation-1",
        }),
        attachments: [],
      })}\n`,
    );
    expect(JSON.parse(await firstResponses.next())).toEqual({
      type: "event",
      event: {
        type: "trial_started",
        request_id: "request-1",
        target: "trial-1",
        conversation_id: "conversation-1",
      },
    });
    expect(JSON.parse(await first.events.next())).toEqual({
      type: "command_ack",
      command_id: "started-command-1",
    });

    first.child.kill("SIGKILL");
    await new Promise((resolve) => first.child.once("exit", resolve));
    firstClient.destroy();

    const second = startWorker(socketPath, stateDir);
    expect(JSON.parse(await second.events.next())).toMatchObject({
      type: "connected",
      subject: socketPath,
    });
    expect(JSON.parse(await second.events.next())).toMatchObject({
      type: "message",
      target: "trial-1",
      message_id: expect.stringMatching(/^request-1:resume:/),
    });

    const secondClient = await connect(socketPath);
    const responses = new LineQueue(secondClient);
    secondClient.write(`${JSON.stringify(request)}\n`);
    expect(JSON.parse(await responses.next())).toEqual({
      type: "event",
      event: {
        type: "trial_started",
        request_id: "request-1",
        target: "trial-1",
        conversation_id: "conversation-1",
      },
    });
    const workerInput = second.child.stdin;
    if (workerInput === null) {
      throw new Error("trial worker stdin is not piped");
    }
    workerInput.write(
      `${JSON.stringify({
        type: "send_message",
        id: "command-1",
        target: "trial-1",
        text: JSON.stringify({
          type: "trial_complete",
          request_id: "request-1",
          target: "trial-1",
          conversation_id: "conversation-1",
          snapshot_id: "snapshot-1",
          summary: "done",
        }),
        attachments: [],
      })}\n`,
    );
    expect(JSON.parse(await responses.next())).toEqual({
      type: "response",
      event: {
        type: "trial_complete",
        request_id: "request-1",
        target: "trial-1",
        conversation_id: "conversation-1",
        snapshot_id: "snapshot-1",
        summary: "done",
      },
    });
    expect(JSON.parse(await second.events.next())).toEqual({
      type: "command_ack",
      command_id: "command-1",
    });
    secondClient.end();

    const retry = await connect(socketPath);
    const retryResponses = new LineQueue(retry);
    retry.write(`${JSON.stringify(request)}\n`);
    expect(JSON.parse(await retryResponses.next())).toEqual({
      type: "response",
      event: {
        type: "trial_complete",
        request_id: "request-1",
        target: "trial-1",
        conversation_id: "conversation-1",
        snapshot_id: "snapshot-1",
        summary: "done",
      },
    });
    retry.end();

    const feedback = {
      type: "trial_feedback",
      request_id: "feedback-1",
      target: "trial-1",
      instructions: "Extract reusable lessons.",
      feedback: "The verifier found an edge case.",
    };
    const feedbackClient = await connect(socketPath);
    const feedbackResponses = new LineQueue(feedbackClient);
    feedbackClient.write(`${JSON.stringify(feedback)}\n`);
    expect(JSON.parse(await second.events.next())).toMatchObject({
      type: "message",
      target: "trial-1",
      message_id: "feedback-1",
      metadata: { type: "trial_feedback", request_id: "feedback-1" },
      text: expect.stringContaining("The verifier found an edge case."),
    });
    workerInput.write(
      `${JSON.stringify({
        type: "send_message",
        id: "feedback-started-command",
        target: "trial-1",
        text: JSON.stringify({
          type: "feedback_started",
          request_id: "feedback-1",
          target: "trial-1",
          conversation_id: "conversation-1",
          sandbox_id: "sandbox-2",
        }),
        attachments: [],
      })}\n`,
    );
    expect(JSON.parse(await feedbackResponses.next())).toMatchObject({
      type: "event",
      event: { type: "feedback_started", sandbox_id: "sandbox-2" },
    });
    expect(JSON.parse(await second.events.next())).toEqual({
      type: "command_ack",
      command_id: "feedback-started-command",
    });
    workerInput.write(
      `${JSON.stringify({
        type: "send_message",
        id: "feedback-complete-command",
        target: "trial-1",
        text: JSON.stringify({
          type: "feedback_complete",
          request_id: "feedback-1",
          target: "trial-1",
          conversation_id: "conversation-1",
          summary: "learned",
        }),
        attachments: [],
      })}\n`,
    );
    expect(JSON.parse(await feedbackResponses.next())).toMatchObject({
      type: "response",
      event: { type: "feedback_complete", summary: "learned" },
    });
    expect(JSON.parse(await second.events.next())).toEqual({
      type: "command_ack",
      command_id: "feedback-complete-command",
    });
    feedbackClient.end();
  }, 10_000);
});

function startWorker(
  socketPath: string,
  stateDir: string,
): {
  child: ReturnType<typeof spawn>;
  events: LineQueue;
} {
  const child = spawn(
    process.execPath,
    [
      path.resolve("node_modules/tsx/dist/cli.mjs"),
      path.resolve("exo/adapters/trial/worker.ts"),
    ],
    {
      cwd: path.resolve("."),
      env: {
        ...process.env,
        EXO_ADAPTER_ID: "adapter-1",
        EXO_ADAPTER_TYPE: "trial",
        EXO_ADAPTER_STATE_DIR: stateDir,
        EXO_ADAPTER_CONFIG: JSON.stringify({ socketPath }),
      },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  children.push(child);
  return { child, events: new LineQueue(child.stdout) };
}

function connect(socketPath: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.setEncoding("utf8");
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}
