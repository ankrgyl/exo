import { describe, expect, it } from "vitest";

import type { SandboxProcess } from "@exo/harness";

import {
  WarmJsonlSandboxWorker,
  WarmJsonlSandboxWorkerTimeoutError,
} from "./shared";

describe("WarmJsonlSandboxWorker", () => {
  it("bounds requests with an actionable timeout", async () => {
    const { process, writes } = idleSandboxProcess();
    const worker = new WarmJsonlSandboxWorker<{ prompt: string }, unknown>({
      name: "test worker",
      parseEvent: JSON.parse,
      process,
    });

    const request = worker.request({ prompt: "hello" }, () => undefined, {
      timeoutMs: 10,
    });
    await expect(request).rejects.toMatchObject({
      name: "WarmJsonlSandboxWorkerTimeoutError",
      phase: "request",
      timeoutMs: 10,
    } satisfies Partial<WarmJsonlSandboxWorkerTimeoutError>);
    await expect(request).rejects.toThrow("test worker timed out after 10ms");
    expect(writes).toEqual(['{"prompt":"hello"}\n']);
  });

  it("uses a separate timeout for the first worker event", async () => {
    const { process } = idleSandboxProcess();
    const worker = new WarmJsonlSandboxWorker<{ prompt: string }, unknown>({
      name: "test worker",
      parseEvent: JSON.parse,
      process,
    });

    const request = worker.request({ prompt: "hello" }, () => undefined, {
      startupTimeoutMs: 10,
      timeoutMs: 1_000,
    });
    await expect(request).rejects.toMatchObject({
      name: "WarmJsonlSandboxWorkerTimeoutError",
      phase: "startup",
      timeoutMs: 10,
    } satisfies Partial<WarmJsonlSandboxWorkerTimeoutError>);
    await expect(request).rejects.toThrow(
      "test worker produced no events within 10ms",
    );
  });
});

function idleSandboxProcess(): {
  process: SandboxProcess;
  writes: string[];
} {
  const writes: string[] = [];
  return {
    writes,
    process: {
      reused: false,
      stdout: new ReadableStream<string>(),
      stderr: new ReadableStream<string>(),
      writeStdin: async (data) => {
        writes.push(data);
      },
      closeStdin: async () => {},
      close: async () => {},
      wait: async () => null,
    },
  };
}
