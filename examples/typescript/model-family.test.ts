import { describe, expect, it } from "vitest";

import { modelFamily } from "./model-family";

describe("modelFamily", () => {
  it("recognises Anthropic model ids", () => {
    expect(modelFamily("claude-sonnet-4-6")).toBe("anthropic");
    expect(modelFamily("claude-opus-4-1-20250805")).toBe("anthropic");
  });

  it("recognises Anthropic ids behind provider prefixes", () => {
    expect(modelFamily("anthropic/claude-sonnet-4-6")).toBe("anthropic");
    expect(modelFamily("us.anthropic.claude-sonnet-4-6-v1:0")).toBe(
      "anthropic",
    );
    expect(modelFamily("publishers/anthropic/models/claude-opus-4-1")).toBe(
      "anthropic",
    );
  });

  it("recognises OpenAI model ids", () => {
    expect(modelFamily("gpt-5.4")).toBe("openai");
    expect(modelFamily("gpt-5.6-terra")).toBe("openai");
    expect(modelFamily("codex-mini-latest")).toBe("openai");
    expect(modelFamily("openai/gpt-5.5")).toBe("openai");
  });

  it("is case and whitespace insensitive", () => {
    expect(modelFamily("  Claude-Sonnet-4-6 ")).toBe("anthropic");
    expect(modelFamily("GPT-5.4")).toBe("openai");
  });

  it("reports unknown for router aliases and unrecognised models", () => {
    // `exo model register auto` is a real configuration (see
    // scripts/agent-harness-e2e.ts): the family genuinely is not statically
    // knowable, and callers must handle that rather than guess.
    expect(modelFamily("auto")).toBe("unknown");
    expect(modelFamily("llama-3.1-70b")).toBe("unknown");
    expect(modelFamily("gemini-2.5-pro")).toBe("unknown");
    expect(modelFamily("")).toBe("unknown");
  });

  it("does not match models that merely contain the substring", () => {
    expect(modelFamily("claudette-7b")).toBe("unknown");
  });
});
