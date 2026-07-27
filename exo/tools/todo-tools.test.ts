import { describe, expect, it } from "vitest";

import type { ArtifactVersion, JsonObject, TurnContext } from "@exo/harness";

import { createTodoToolInstances, todoInstruction } from "./todo-tools";

// Minimal in-memory stand-in for an artifact-backed handle (agent or
// conversation). Each writeArtifactJson appends a new version at the same path,
// matching how the real store versions artifacts.
class FakeHandle {
  private versions: {
    artifactId: string;
    path: string;
    version: number;
    value: unknown;
  }[] = [];
  private seq = 0;

  async listArtifacts(): Promise<ArtifactVersion[]> {
    return this.versions.map(({ artifactId, path, version }) => ({
      artifactId,
      path,
      version,
      createdAt: "1970-01-01T00:00:00Z",
      sizeBytes: 0,
    }));
  }

  async readArtifactJson<T>({
    artifactId,
    version,
  }: {
    artifactId: string;
    version?: number;
  }): Promise<T | null> {
    const selected = this.versions.find(
      (item) =>
        item.artifactId === artifactId &&
        (version === undefined || item.version === version),
    );
    return selected ? (selected.value as T) : null;
  }

  async writeArtifactJson({
    path,
    value,
  }: {
    path: string;
    value: unknown;
  }): Promise<ArtifactVersion> {
    this.seq += 1;
    const version = this.seq;
    const artifactId = `${path}@${version}`;
    this.versions.push({ artifactId, path, version, value });
    return {
      artifactId,
      path,
      version,
      createdAt: "1970-01-01T00:00:00Z",
      sizeBytes: 0,
    };
  }
}

function makeContext(): { context: TurnContext; conversation: FakeHandle } {
  const conversation = new FakeHandle();
  const context = {
    exoharness: { current: { conversation } },
  } as unknown as TurnContext;
  return { context, conversation };
}

const todowrite = createTodoToolInstances()[0];

function call(args: JsonObject, context: TurnContext) {
  return todowrite.handler.execute(args, { context });
}

describe("todo tools", () => {
  it("writes a list and injects it into the prompt", async () => {
    const { context } = makeContext();
    const result = (await call(
      {
        todos: [
          { content: "read the code", status: "completed" },
          { content: "write the fix", status: "in_progress" },
          { content: "run the tests", status: "pending" },
        ],
      },
      context,
    )) as { ok: boolean; total: number; remaining: number };

    expect(result.ok).toBe(true);
    expect(result.total).toBe(3);
    expect(result.remaining).toBe(2);

    const message = await todoInstruction(context);
    expect(message).not.toBeNull();
    const content = String(message?.content);
    expect(content).toContain("[in_progress] write the fix");
    expect(content).toContain("[pending] run the tests");
    expect(content).toContain("[completed] read the code");
  });

  it("replaces the whole list on each call", async () => {
    const { context } = makeContext();
    await call(
      { todos: [{ content: "old plan", status: "pending" }] },
      context,
    );
    await call(
      { todos: [{ content: "new plan", status: "in_progress" }] },
      context,
    );

    const content = String((await todoInstruction(context))?.content);
    expect(content).toContain("new plan");
    expect(content).not.toContain("old plan");
  });

  it("returns null when the list is empty or fully done", async () => {
    const { context } = makeContext();
    expect(await todoInstruction(context)).toBeNull();

    await call(
      {
        todos: [
          { content: "done step", status: "completed" },
          { content: "abandoned step", status: "cancelled" },
        ],
      },
      context,
    );
    expect(await todoInstruction(context)).toBeNull();
  });

  it("clears the list when given an empty array", async () => {
    const { context } = makeContext();
    await call(
      { todos: [{ content: "in flight", status: "in_progress" }] },
      context,
    );
    const cleared = (await call({ todos: [] }, context)) as {
      ok: boolean;
      total: number;
    };
    expect(cleared.ok).toBe(true);
    expect(cleared.total).toBe(0);
    expect(await todoInstruction(context)).toBeNull();
  });

  it("rejects malformed items, empty content, and oversized lists", async () => {
    const { context } = makeContext();
    const badStatus = (await call(
      { todos: [{ content: "x", status: "someday" }] },
      context,
    )) as { ok: boolean };
    expect(badStatus.ok).toBe(false);

    const emptyContent = (await call(
      { todos: [{ content: "   ", status: "pending" }] },
      context,
    )) as { ok: boolean };
    expect(emptyContent.ok).toBe(false);

    const tooMany = (await call(
      {
        todos: Array.from({ length: 51 }, (_, i) => ({
          content: `step ${i}`,
          status: "pending",
        })),
      },
      context,
    )) as { ok: boolean };
    expect(tooMany.ok).toBe(false);

    // Nothing was persisted by the rejected calls.
    expect(await todoInstruction(context)).toBeNull();
  });

  it("treats a corrupt stored list as empty instead of throwing", async () => {
    const { context, conversation } = makeContext();
    await conversation.writeArtifactJson({
      path: "todos/exo-todos.json",
      value: { not: "a list" },
    });
    expect(await todoInstruction(context)).toBeNull();
  });

  it("treats a failing listArtifacts as empty instead of throwing", async () => {
    const { context, conversation } = makeContext();
    conversation.listArtifacts = async () => {
      throw new Error("transient rpc failure");
    };
    expect(await todoInstruction(context)).toBeNull();
  });
});
