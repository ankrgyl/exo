import { describe, expect, it } from "vitest";

import { parsePiModelReference, toPiJson } from "./protocol";

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

describe("toPiJson", () => {
  it("normalizes bigint and circular values", () => {
    const value: { count: bigint; self?: unknown } = { count: 3n };
    value.self = value;
    expect(toPiJson(value)).toEqual({ count: "3", self: "[circular]" });
  });
});
