import { describe, expect, it } from "vitest";

import type { ArtifactVersion, JsonObject, TurnContext } from "@exo/harness";

import { createMemoryToolInstances, memoryInstruction } from "./memory-tools";

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

function makeContext(): { context: TurnContext; agent: FakeHandle } {
  const agent = new FakeHandle();
  const context = {
    exoharness: { current: { agent } },
  } as unknown as TurnContext;
  return { context, agent };
}

const toolsByName = Object.fromEntries(
  createMemoryToolInstances().map((tool) => [tool.definition.name, tool]),
);

function call(name: string, args: JsonObject, context: TurnContext) {
  return toolsByName[name].handler.execute(args, { context });
}

describe("memory tools", () => {
  it("remembers a fact and injects it into the prompt", async () => {
    const { context } = makeContext();
    const result = (await call(
      "remember",
      { text: "favorite coffee is a flat white" },
      context,
    )) as { ok: boolean; id: string };

    expect(result.ok).toBe(true);

    const message = await memoryInstruction(context);
    expect(message).not.toBeNull();
    const content = String(message?.content);
    expect(content).toContain("favorite coffee is a flat white");
    expect(content).toContain(result.id);
  });

  it("appends across calls (read-modify-write, not overwrite)", async () => {
    const { context } = makeContext();
    await call("remember", { text: "fact one" }, context);
    const second = (await call("remember", { text: "fact two" }, context)) as {
      total: number;
    };

    expect(second.total).toBe(2);
    const content = String((await memoryInstruction(context))?.content);
    expect(content).toContain("fact one");
    expect(content).toContain("fact two");
  });

  it("forgets a fact by id", async () => {
    const { context } = makeContext();
    const saved = (await call(
      "remember",
      { text: "temporary fact" },
      context,
    )) as { id: string };

    const forgotten = (await call("forget", { id: saved.id }, context)) as {
      ok: boolean;
      removed: number;
    };
    expect(forgotten.ok).toBe(true);
    expect(forgotten.removed).toBe(1);

    expect(await memoryInstruction(context)).toBeNull();
  });

  it("rejects empty and oversized text", async () => {
    const { context } = makeContext();
    const empty = (await call("remember", { text: "   " }, context)) as {
      ok: boolean;
    };
    expect(empty.ok).toBe(false);

    const huge = (await call(
      "remember",
      { text: "x".repeat(1000) },
      context,
    )) as { ok: boolean };
    expect(huge.ok).toBe(false);
  });

  it("returns null when nothing is remembered", async () => {
    const { context } = makeContext();
    expect(await memoryInstruction(context)).toBeNull();
  });

  it("makes the write path (remember) reject on a corrupt store", async () => {
    const { context, agent } = makeContext();
    // A payload that exists but does not match the schema.
    await agent.writeArtifactJson({
      path: "memory/exo-memory.json",
      value: { entries: "not-an-array" },
    });
    await expect(call("remember", { text: "x" }, context)).rejects.toThrow(
      /corrupt memory artifact/,
    );
  });

  it("degrades the read path on a corrupt store instead of throwing", async () => {
    const { context, agent } = makeContext();
    await agent.writeArtifactJson({
      path: "memory/exo-memory.json",
      value: { entries: "not-an-array" },
    });
    // Prompt assembly must not throw — it returns a degraded note instead.
    const message = await memoryInstruction(context);
    expect(message).not.toBeNull();
    expect(String(message?.content)).toMatch(/unavailable|corrupt/i);
  });
});
