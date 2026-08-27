import { describe, expect, it } from "vitest";

import {
  parsePiModelReference,
  parsePiWorkerEvent,
  toPiJson,
} from "./protocol";

describe("parsePiModelReference", () => {
  it("splits the provider from model ids that contain slashes", () => {
    expect(
      parsePiModelReference("openrouter/anthropic/claude-sonnet-4"),
    ).toEqual({
      provider: "openrouter",
      model: "anthropic/claude-sonnet-4",
    });
  });

  it("requires a provider-qualified model", () => {
    expect(() => parsePiModelReference("claude-sonnet-4-6")).toThrow(
      "provider-qualified",
    );
  });
});

describe("parsePiWorkerEvent", () => {
  it("accepts a request-scoped completed event", () => {
    const event = {
      type: "completed",
      requestId: "request-1",
      result: {
        status: "finished",
        finalText: "done",
        model: "model",
        provider: "provider",
        usage: {
          input: 1,
          output: 1,
          cacheRead: 0,
          cacheWrite: 0,
          totalTokens: 2,
          cost: 0,
        },
        durationMs: 10,
      },
    };
    expect(parsePiWorkerEvent(JSON.stringify(event))).toEqual(event);
  });

  it("rejects unknown and malformed worker events", () => {
    expect(() => parsePiWorkerEvent("not-json")).toThrow(
      "invalid Pi sandbox worker event",
    );
    expect(() =>
      parsePiWorkerEvent('{"type":"unknown","requestId":"request-1"}'),
    ).toThrow("invalid Pi sandbox worker event");
    expect(() =>
      parsePiWorkerEvent('{"type":"delta","requestId":"request-1"}'),
    ).toThrow("invalid Pi sandbox worker event");
    expect(() =>
      parsePiWorkerEvent(
        '{"type":"message","requestId":"request-1","message":{},"durationMs":-1}',
      ),
    ).toThrow("invalid Pi sandbox worker event");
  });
});

describe("toPiJson", () => {
  it("normalizes bigint and circular values", () => {
    const value: { count: bigint; self?: unknown } = { count: 3n };
    value.self = value;
    expect(toPiJson(value)).toEqual({ count: "3", self: "[circular]" });
  });
});
