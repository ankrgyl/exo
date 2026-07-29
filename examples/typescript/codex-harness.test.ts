import { describe, expect, it } from "vitest";

import type { Conversation, EventQuery } from "@exo/harness";

import { latestSandboxStartedEventId } from "./codex-harness";

function conversationWith(ids: string[]): {
  handle: Pick<Conversation, "getEvents">;
  queries: EventQuery[];
} {
  const queries: EventQuery[] = [];
  const handle = {
    getEvents: async (query: EventQuery) => {
      queries.push(query);
      return { events: ids.map((id) => ({ id })) };
    },
  } as unknown as Pick<Conversation, "getEvents">;
  return { handle, queries };
}

describe("latestSandboxStartedEventId", () => {
  it("returns the newest sandbox_started event id", async () => {
    const { handle, queries } = conversationWith(["evt-9", "evt-3"]);
    await expect(latestSandboxStartedEventId(handle)).resolves.toBe("evt-9");
    expect(queries[0].types).toEqual(["sandbox_started"]);
    expect(queries[0].direction).toBe("desc");
  });

  it("returns null when the sandbox was never started", async () => {
    const { handle } = conversationWith([]);
    await expect(latestSandboxStartedEventId(handle)).resolves.toBeNull();
  });
});
