import { describe, expect, it } from "vitest";
import type { EventData } from "@exo/harness";

import { shouldRetryEmptyCompletion } from "./turn-loop";

// shouldRetryEmptyCompletion only reads array lengths and counters, so tests
// build a minimal event shape and cast to EventData.
function textEvent(): EventData {
  return { type: "messages", messages: [] } as unknown as EventData;
}

describe("shouldRetryEmptyCompletion", () => {
  it("retries when the round produced no events and no tool calls", () => {
    expect(shouldRetryEmptyCompletion([], [], 0, 2)).toBe(true);
    expect(shouldRetryEmptyCompletion([], [], 1, 2)).toBe(true);
  });

  it("stops once the retry budget is spent", () => {
    expect(shouldRetryEmptyCompletion([], [], 2, 2)).toBe(false);
    expect(shouldRetryEmptyCompletion([], [], 3, 2)).toBe(false);
  });

  it("never retries when the budget is zero", () => {
    expect(shouldRetryEmptyCompletion([], [], 0, 0)).toBe(false);
  });

  it("does not treat a round with text events as an empty completion", () => {
    expect(shouldRetryEmptyCompletion([textEvent()], [], 0, 2)).toBe(false);
  });

  it("does not treat a round with tool calls as an empty completion", () => {
    expect(shouldRetryEmptyCompletion([], [{}], 0, 2)).toBe(false);
  });
});
