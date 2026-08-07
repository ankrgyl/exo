import { describe, expect, it } from "vitest";

import {
  assistantTextMessage,
  materializeEventsToMessages,
  messagesEvent,
  projectAnthropicMessageToolEvents,
  toolRequestedEvent,
  toolResultEvent,
  userTextMessage,
  type Event,
} from "../typescript/harness";

describe("agent harness canonical events", () => {
  it("replays message and tool events into portable conversation messages", () => {
    const events: Event[] = [
      event("e1", messagesEvent([userTextMessage("inspect the repo")])),
      event(
        "e2",
        toolRequestedEvent({
          toolCallId: "tool-1",
          request: {
            functionName: "codex.shell",
            arguments: { command: "pwd" },
          },
        }),
      ),
      event(
        "e3",
        toolResultEvent("tool-1", {
          exit_code: 0,
          stdout: "/workspace\n",
          stderr: "",
        }),
      ),
      event("e4", messagesEvent([assistantTextMessage("done")])),
    ];

    expect(materializeEventsToMessages(events)).toEqual([
      userTextMessage("inspect the repo"),
      {
        role: "tool",
        content: [
          {
            type: "tool_result",
            tool_call_id: "tool-1",
            tool_name: "codex.shell",
            output: {
              exit_code: 0,
              stdout: "/workspace\n",
              stderr: "",
            },
          },
        ],
      },
      assistantTextMessage("done"),
    ]);
  });

  it("projects Claude/Cursor-style tool_use and tool_result blocks", () => {
    const requested = projectAnthropicMessageToolEvents(
      {
        type: "assistant",
        message: {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "tool-1",
              name: "Bash",
              input: { command: "ls" },
            },
          ],
        },
      },
      { toolNamePrefix: "claude." },
    );
    const result = projectAnthropicMessageToolEvents({
      type: "user",
      message: {
        role: "user",
        content: [
          {
            type: "tool_result",
            tool_use_id: "tool-1",
            content: "README.md\n",
          },
        ],
      },
    });

    expect(requested).toEqual([
      toolRequestedEvent({
        toolCallId: "tool-1",
        request: {
          functionName: "claude.Bash",
          arguments: { command: "ls" },
        },
      }),
    ]);
    expect(result).toEqual([
      toolResultEvent("tool-1", {
        content: "README.md\n",
        is_error: false,
      }),
    ]);
  });
});

function event(id: string, data: Event["data"]): Event {
  return {
    id,
    conversationId: "conversation-1",
    sessionId: "session-1",
    turnId: "turn-1",
    createdAt: "2026-05-03T00:00:00Z",
    data,
  };
}
