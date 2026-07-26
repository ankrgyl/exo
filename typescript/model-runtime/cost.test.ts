import { describe, expect, it } from "vitest";

import { computeCostUsd, lookup, maxInputTokens, parseTable } from "./cost";

const FIXTURE = `{
  "sample_spec": { "comment": "ignored" },
  "claude-sonnet-4-6": {
    "litellm_provider": "anthropic", "input_cost_per_token": 3e-06,
    "output_cost_per_token": 1.5e-05, "cache_read_input_token_cost": 3e-07,
    "cache_creation_input_token_cost": 3.75e-06,
    "max_input_tokens": 200000
  },
  "gpt-4o-mini": {
    "litellm_provider": "openai", "input_cost_per_token": 1.5e-07,
    "output_cost_per_token": 6e-07, "cache_read_input_token_cost": 7.5e-08,
    "max_input_tokens": 128000
  },
  "gpt-4": { "litellm_provider": "openai", "input_cost_per_token": 3e-05 },
  "us.anthropic.claude-sonnet-4-6": {
    "litellm_provider": "bedrock_converse", "input_cost_per_token": 3.3e-06,
    "output_cost_per_token": 1.65e-05, "cache_read_input_token_cost": 3.3e-07
  }
}`;

const table = parseTable(FIXTURE);

describe("cost", () => {
  it("skips sample_spec", () => {
    expect(table.size).toBe(4);
  });

  it("resolves dated revisions, not neighbors", () => {
    expect(lookup(table, "claude-sonnet-4-6-20251022")?.litellm_provider).toBe(
      "anthropic",
    );
    expect(lookup(table, "gpt-4o")).toBeUndefined();
    expect(lookup(table, "gpt-4-0613")).toBeDefined();
  });

  it("bills Anthropic additively (prompt excludes cached)", () => {
    // 500 fresh + 10k read + 200 out = 0.0015 + 0.003 + 0.003
    expect(
      computeCostUsd(table, "claude-sonnet-4-6", {
        prompt: 500,
        completion: 200,
        cached: 10_000,
      }),
    ).toBeCloseTo(0.0075, 12);
  });

  it("bills OpenAI inclusively (subtract cached)", () => {
    expect(
      computeCostUsd(table, "gpt-4o-mini", {
        prompt: 2_000,
        completion: 1_000,
        cached: 500,
      }),
    ).toBeCloseTo(0.0008625, 12);
  });

  it("treats Bedrock as inclusive, not additive", () => {
    const expected = 1_500 * 3.3e-6 + 500 * 3.3e-7 + 1_000 * 1.65e-5;
    expect(
      computeCostUsd(table, "us.anthropic.claude-sonnet-4-6", {
        prompt: 2_000,
        completion: 1_000,
        cached: 500,
      }),
    ).toBeCloseTo(expected, 12);
  });

  it("returns null for unknown models", () => {
    expect(
      computeCostUsd(table, "acme-llm-9000", { prompt: 100, completion: 50 }),
    ).toBeNull();
  });
});

describe("maxInputTokens", () => {
  it("reads the model's input limit", () => {
    expect(maxInputTokens(table, "claude-sonnet-4-6")).toBe(200_000);
    expect(maxInputTokens(table, "gpt-4o-mini")).toBe(128_000);
  });

  it("resolves through the same prefix matching as pricing", () => {
    expect(maxInputTokens(table, "claude-sonnet-4-6-20251022")).toBe(200_000);
  });

  it("returns null when the model is unknown", () => {
    expect(maxInputTokens(table, "acme-llm-9000")).toBeNull();
  });

  it("returns null when the entry omits the limit", () => {
    // `gpt-4` and the bedrock entry are priced but carry no max_input_tokens.
    // Callers must fall back rather than treat a missing limit as zero.
    expect(maxInputTokens(table, "gpt-4")).toBeNull();
    expect(maxInputTokens(table, "us.anthropic.claude-sonnet-4-6")).toBeNull();
  });
});
