import { describe, it, expect, vi } from "vitest";
import type { TurnContext } from "@exo/harness";

// The loop pulls prompt messages, the tool registry, turn metadata, and response
// parsing from these modules. Stub the heavy pieces so a fake runtime can drive
// the loop and we can observe which tool calls actually execute.
vi.mock("@exo/harness", async (importOriginal) => {
  const actual = (await importOriginal()) as Record<string, unknown>;
  return {
    ...actual,
    materializePromptMessages: async () => [],
    turnMetadata: () => ({}),
    createToolRegistry: () => ({
      register() {
        return this;
      },
      definitions: () => [],
      executePending: async () => [],
      get: () => undefined,
    }),
  };
});

vi.mock("@exo/model-runtime/responses", async (importOriginal) => {
  const actual = (await importOriginal()) as Record<string, unknown>;
  return {
    ...actual,
    responseToLinguaEvents: (r: { __events?: unknown[] } | undefined) =>
      r?.__events ?? [],
    responseToolCalls: (r: { __toolCalls?: unknown[] } | undefined) =>
      r?.__toolCalls ?? [],
  };
});

// Imported after the mocks are registered (vi.mock is hoisted to the top).
import { runResponsesTurnLoop } from "./turn-loop";

function sendResponse(id: string) {
  return {
    __events: [],
    __toolCalls: [
      {
        toolCallId: id,
        request: { functionName: "send_adapter_message", arguments: {} },
      },
    ],
  };
}
function shellResponse(id: string) {
  return {
    __events: [],
    __toolCalls: [
      { toolCallId: id, request: { functionName: "shell", arguments: {} } },
    ],
  };
}
function textResponse(content: string) {
  return {
    __events: [
      { type: "messages", messages: [{ role: "assistant", content }] },
    ],
    __toolCalls: [],
  };
}

// Drive the loop through a fixed response sequence; return the function names of
// the tool calls that actually executed (skipped duplicates never reach here).
async function runWith(sequence: unknown[]): Promise<string[]> {
  let call = 0;
  const executed: string[] = [];
  const runtime = {
    complete: async () => sequence[call++],
    completeStream: async () => sequence[call++],
    traceToolCall: async (
      _parent: unknown,
      _ctx: unknown,
      toolCall: { toolCallId: string; request: { functionName: string } },
    ) => {
      executed.push(toolCall.request.functionName);
      return [
        {
          type: "tool_result",
          tool_call_id: toolCall.toolCallId,
          result: { code: 0 },
        },
      ];
    },
  };
  const context = {
    streaming: false,
    request: { input: [] },
    agentConfig: {
      maxToolRoundTrips: null,
      maxOutputTokens: null,
      instructions: [],
      enableAgentToolCreation: false,
      typescript: {},
    },
    exoharness: {
      current: {
        conversation: {},
        turn: { addEvents: async () => ({ latestEventId: "e" }) },
      },
    },
  } as unknown as TurnContext;

  await runResponsesTurnLoop(
    runtime as never,
    context,
    {} as never,
    "test-model",
    { registerTools: async () => {} },
  );
  return executed;
}

describe("runResponsesTurnLoop duplicate-send guard", () => {
  it("skips a repeated send when nothing was done in between, and ends the turn", async () => {
    // round 0 sends; round 1 sends again with no work between → duplicate
    const executed = await runWith([sendResponse("s1"), sendResponse("s2")]);
    const sends = executed.filter((f) => f === "send_adapter_message");
    expect(sends).toHaveLength(1); // only the first reply goes out
  });

  it("allows a second send when real work happened in between", async () => {
    // send (acknowledge) → shell (real work) → send (report) → done
    const executed = await runWith([
      sendResponse("s1"),
      shellResponse("sh1"),
      sendResponse("s2"),
      textResponse("done"),
    ]);
    const sends = executed.filter((f) => f === "send_adapter_message");
    expect(sends).toHaveLength(2); // acknowledge → work → report all go through
    expect(executed).toContain("shell");
  });
});
