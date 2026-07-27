import { describe, expect, it } from "vitest";

import { linguaMessagesToResponsesInput } from "./responses";

// Exact message shapes as materialized from the event log after a reasoning
// model's tool round (reasoning and function_call arrive as separate
// assistant messages whose ids are server-side item ids).
const REPLAYED_MESSAGES = [
  { role: "user", content: "run it" },
  {
    role: "assistant",
    content: [{ type: "reasoning", text: "" }],
    id: "rs_0ac274d81e8f3558016a2bacb119f48194bf6328d6f9de586f",
  },
  {
    role: "assistant",
    content: [
      {
        type: "tool_call",
        tool_call_id: "call_h1Z6GgiNspA0723ZxkfDcuIr",
        tool_name: "shell",
        arguments: { type: "valid", value: { command: "ls" } },
      },
    ],
    id: "fc_0ac274d81e8f3558016a2bacb186a88194ba78d03f57b132d9",
  },
] as never;

describe("linguaMessagesToResponsesInput stateless replay", () => {
  it("drops reasoning items and strips server item ids", () => {
    const input = linguaMessagesToResponsesInput(REPLAYED_MESSAGES);
    const types = input.map((item) => (item as { type?: string }).type);
    expect(types).not.toContain("reasoning");
    for (const item of input) {
      expect((item as { id?: unknown }).id).toBeUndefined();
    }
    const fc = input.find(
      (item) => (item as { type?: string }).type === "function_call",
    ) as { call_id?: string } | undefined;
    expect(fc?.call_id).toBe("call_h1Z6GgiNspA0723ZxkfDcuIr");
  });
});
