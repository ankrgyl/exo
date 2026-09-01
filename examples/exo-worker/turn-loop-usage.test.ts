import { describe, expect, it } from "vitest";

import {
  extractProviderUsage,
  PROVIDER_USAGE_EVENT_TYPE,
  providerUsageEvent,
} from "./provider-usage.js";

describe("extractProviderUsage", () => {
  it("reads Responses-style cache and reasoning details", () => {
    expect(
      extractProviderUsage({
        usage: {
          input_tokens: 1000,
          output_tokens: 20,
          input_tokens_details: { cached_tokens: 800 },
          output_tokens_details: { reasoning_tokens: 5 },
        },
      }),
    ).toEqual({
      promptTokens: 1000,
      completionTokens: 20,
      cachedTokens: 800,
      reasoningTokens: 5,
    });
  });

  it("reads Chat Completions-style cache and reasoning details", () => {
    expect(
      extractProviderUsage({
        usage: {
          prompt_tokens: 1000,
          completion_tokens: 20,
          prompt_tokens_details: { cached_tokens: 800 },
          completion_tokens_details: { reasoning_tokens: 5 },
        },
      }),
    ).toEqual({
      promptTokens: 1000,
      completionTokens: 20,
      cachedTokens: 800,
      reasoningTokens: 5,
    });
  });
});

describe("providerUsageEvent", () => {
  it("emits a billable custom event for compression calls", () => {
    expect(
      providerUsageEvent(
        {
          promptTokens: 200_000,
          completionTokens: 800,
          cachedTokens: 150_000,
          reasoningTokens: 40,
        },
        "grok-4.6",
        "compression",
      ),
    ).toEqual({
      type: "custom",
      event_type: PROVIDER_USAGE_EVENT_TYPE,
      payload: {
        model: "grok-4.6",
        source: "compression",
        prompt_tokens: 200_000,
        completion_tokens: 800,
        prompt_cached_tokens: 150_000,
        completion_reasoning_tokens: 40,
      },
    });
  });
});
