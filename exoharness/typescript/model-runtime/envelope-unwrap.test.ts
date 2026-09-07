import { describe, expect, it } from "vitest";

import { messagesToChatMessages } from "./responses";

// History as materialized from the event log: tool-call arguments arrive in
// lingua's typed-value envelope {"type":"valid","value":{...}} (see
// responses-input.test.ts for the same shape on the Responses path).
const HISTORY = [
  { role: "user", content: "run it" },
  {
    role: "assistant",
    content: [
      {
        type: "tool_call",
        tool_call_id: "call_1",
        tool_name: "shell",
        arguments: { type: "valid", value: { command: "ls" } },
      },
    ],
  },
  {
    role: "assistant",
    content: [
      {
        type: "tool_call",
        tool_call_id: "call_2",
        tool_name: "shell",
        // A model echoing its own wrapped arguments from history can nest
        // envelopes; the harness must strip every layer.
        arguments: {
          type: "valid",
          value: { type: "valid", value: { command: "pwd" } },
        },
      },
    ],
  },
] as never;

describe("chat history tool-call arguments", () => {
  it("replays typed-value envelopes unwrapped so models do not imitate them", () => {
    const messages = messagesToChatMessages(HISTORY);
    const toolCalls = messages.flatMap(
      (message) =>
        (message as { tool_calls?: { function: { arguments: string } }[] })
          .tool_calls ?? [],
    );
    expect(toolCalls).toHaveLength(2);
    expect(JSON.parse(toolCalls[0].function.arguments)).toEqual({
      command: "ls",
    });
    expect(JSON.parse(toolCalls[1].function.arguments)).toEqual({
      command: "pwd",
    });
  });
});
