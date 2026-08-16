import { describe, expect, it } from "vitest";

import { piResultUsageRecord, projectPiAssistantMessage } from "./projection";

describe("projectPiAssistantMessage", () => {
  it("projects text and standard Exoharness usage", () => {
    const projection = projectPiAssistantMessage(
      {
        role: "assistant",
        responseId: "response-1",
        model: "claude-sonnet-4-6",
        content: [
          { type: "thinking", thinking: "private" },
          { type: "text", text: "hello " },
          { type: "text", text: "world" },
        ],
        usage: {
          input: 12,
          output: 5,
          cacheRead: 3,
          cacheWrite: 2,
          reasoning: 1,
          cost: { total: 0.004 },
        },
      },
      {
        upstreamModel: "anthropic/fallback",
        durationMs: 125,
        ttftMs: 42,
      },
    );

    expect(projection).toEqual({
      text: "hello world",
      usage: {
        model: "claude-sonnet-4-6",
        prompt_tokens: 12,
        completion_tokens: 5,
        prompt_cached_tokens: 3,
        prompt_cache_creation_tokens: 2,
        completion_reasoning_tokens: 1,
        cost_usd: 0.004,
        duration_ms: 125,
        ttft_ms: 42,
      },
    });
  });

  it("ignores non-assistant messages", () => {
    expect(
      projectPiAssistantMessage(
        { role: "toolResult", content: [] },
        { upstreamModel: "model" },
      ),
    ).toBeNull();
  });
});

describe("piResultUsageRecord", () => {
  it("projects aggregate fallback usage and duration", () => {
    expect(
      piResultUsageRecord(
        {
          status: "finished",
          finalText: "done",
          model: "model",
          provider: "provider",
          usage: {
            input: 10,
            output: 4,
            cacheRead: 2,
            cacheWrite: 1,
            reasoning: 3,
            totalTokens: 17,
            cost: 0.02,
          },
          durationMs: 1250,
        },
        25,
      ),
    ).toEqual({
      model: "model",
      prompt_tokens: 10,
      completion_tokens: 4,
      prompt_cached_tokens: 2,
      prompt_cache_creation_tokens: 1,
      completion_reasoning_tokens: 3,
      cost_usd: 0.02,
      duration_ms: 1250,
      ttft_ms: 25,
    });
  });
});
