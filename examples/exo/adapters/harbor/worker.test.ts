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
      .createInterface({
        input: stream,
        crlfDelay: Number.POSITIVE_INFINITY,
      })
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

function connect(socketPath: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.setEncoding("utf8");
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}

const children: ReturnType<typeof spawn>[] = [];

afterEach(() => {
  for (const child of children.splice(0)) {
    child.kill("SIGKILL");
  }
});

describe("Harbor adapter worker", () => {
  it("routes a correlated response and replays it for duplicate requests", async () => {
    const tempDir = fs.mkdtempSync(
      path.join(os.tmpdir(), "exo-harbor-worker-"),
    );
    const socketPath = path.join(tempDir, "harbor.sock");
    const stateDir = path.join(tempDir, "state");
    const child = spawn(
      process.execPath,
      [
        path.resolve("node_modules/tsx/dist/cli.mjs"),
        path.resolve("examples/exo/adapters/harbor/worker.ts"),
      ],
      {
        cwd: path.resolve("."),
        env: {
          ...process.env,
          EXO_ADAPTER_ID: "adapter-1",
          EXO_ADAPTER_TYPE: "harbor",
          EXO_ADAPTER_STATE_DIR: stateDir,
          EXO_ADAPTER_CONFIG: JSON.stringify({ socketPath }),
        },
        stdio: ["pipe", "pipe", "pipe"],
      },
    );
    children.push(child);
    const workerEvents = new LineQueue(child.stdout);
    expect(JSON.parse(await workerEvents.next())).toMatchObject({
      type: "connected",
      subject: socketPath,
    });

    const request = {
      type: "task_started",
      message_id: "message-1",
      trial_id: "trial-1",
      task_name: "task",
      instruction: "Solve it",
      conversation_id: "conversation-1",
      sandbox_id: "sandbox-1",
    };
    const client = await connect(socketPath);
    const clientLines = new LineQueue(client);
    client.write(`${JSON.stringify(request)}\n`);
    const wakeup = JSON.parse(await workerEvents.next());
    expect(wakeup).toMatchObject({
      type: "message",
      target: "harbor:trial-1:task_started",
      message_id: "message-1",
    });

    child.stdin.write(
      `${JSON.stringify({
        type: "send_message",
        id: "command-1",
        target: wakeup.target,
        text: JSON.stringify({
          type: "task_complete",
          trial_id: "trial-1",
          summary: "done",
        }),
        attachments: [],
      })}\n`,
    );
    expect(JSON.parse(await clientLines.next())).toEqual({
      type: "response",
      event: {
        type: "task_complete",
        trial_id: "trial-1",
        summary: "done",
      },
    });
    expect(JSON.parse(await workerEvents.next())).toEqual({
      type: "command_ack",
      command_id: "command-1",
    });
    client.end();

    const retry = await connect(socketPath);
    const retryLines = new LineQueue(retry);
    retry.write(`${JSON.stringify(request)}\n`);
    expect(JSON.parse(await retryLines.next())).toEqual({
      type: "response",
      event: {
        type: "task_complete",
        trial_id: "trial-1",
        summary: "done",
      },
    });
    retry.end();
  }, 10_000);
});
