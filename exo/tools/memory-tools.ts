import { randomUUID } from "node:crypto";

import type {
  Agent,
  ArtifactVersion,
  HarnessToolRegistry,
  JsonObject,
  JsonValue,
  Message,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

// Durable agent-writable memory: a single global store that persists across
// every conversation for this agent. Stored as a JSON artifact on the agent
// handle, mirroring how config/executor.json is persisted. See
// exo/docs/SELF-CONTROL.md section 2 for the design.
const MEMORY_ARTIFACT_PATH = "memory/exo-memory.json";
// Soft caps so always-injecting memory cannot grow the prompt without bound.
const MAX_ENTRIES = 200;
const MAX_TEXT_CHARS = 600;

interface MemoryEntry {
  id: string;
  text: string;
  createdAt: string;
}

interface MemoryStore {
  entries: MemoryEntry[];
}

// The artifact subset both Agent and the test fake provide.
type MemoryHandle = Pick<
  Agent,
  "listArtifacts" | "readArtifactJson" | "writeArtifactJson"
>;

function memoryHandle(context: TurnContext): MemoryHandle {
  return context.exoharness.current.agent;
}

// Reads and validates the store. Throws on a corrupt artifact (invalid JSON or
// failing the schema) so the write path refuses to bury it; the read/inject
// path catches this in memoryInstruction. A missing artifact is not corrupt —
// it is a legitimately empty store.
async function readMemory(handle: MemoryHandle): Promise<MemoryStore> {
  let raw: unknown;
  try {
    raw = await readLatestMemoryArtifact(handle);
  } catch (cause) {
    // readArtifactJson parses the stored bytes; it throws if they are not
    // valid JSON. Treat that the same as a schema failure below.
    throw new Error(
      `corrupt memory artifact ${MEMORY_ARTIFACT_PATH}: not valid JSON`,
      { cause },
    );
  }
  if (raw === null) {
    return { entries: [] };
  }
  if (!isMemoryStore(raw)) {
    throw new Error(
      `corrupt memory artifact ${MEMORY_ARTIFACT_PATH}: invalid memory store shape`,
    );
  }
  return raw;
}

// TODO(storage-rework): remember/forget are read-modify-write with no
// compare-and-swap. Agent-scoped memory is shared across conversations, so two
// channels running turns concurrently can both read version N and both write
// N+1, losing one update. Fix alongside the artifact versioning rework — either
// an optimistic write that rejects when the latest version moved, or an
// append-entry store op instead of rewriting the whole store.
async function writeMemory(
  handle: MemoryHandle,
  store: MemoryStore,
): Promise<void> {
  await handle.writeArtifactJson({
    path: MEMORY_ARTIFACT_PATH,
    value: store as unknown as JsonValue,
  });
}

function rememberTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "remember",
      description:
        "Save a durable fact so you still have it in future turns and conversations. Use this when the user shares a lasting preference or fact about themselves, or when you want to persist something about yourself. The fact is injected into your context every turn. Phrase it as a standalone statement.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          text: {
            type: "string",
            description:
              'The fact to remember, phrased to stand on its own (e.g. "User\'s favorite coffee is a flat white").',
          },
        },
        required: ["text"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const text = typeof args.text === "string" ? args.text.trim() : "";
        if (text.length === 0) {
          return { ok: false, error: "text is required" };
        }
        if (text.length > MAX_TEXT_CHARS) {
          return {
            ok: false,
            error: `text exceeds ${MAX_TEXT_CHARS} characters; summarize it first`,
          };
        }
        const handle = memoryHandle(execution.context);
        const store = await readMemory(handle);
        const entry: MemoryEntry = {
          id: `mem_${randomUUID().slice(0, 8)}`,
          text,
          createdAt: new Date().toISOString(),
        };
        store.entries.push(entry);
        let dropped = 0;
        if (store.entries.length > MAX_ENTRIES) {
          dropped = store.entries.length - MAX_ENTRIES;
          store.entries = store.entries.slice(-MAX_ENTRIES);
        }
        await writeMemory(handle, store);
        return { ok: true, id: entry.id, total: store.entries.length, dropped };
      },
    },
  };
}

function forgetTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "forget",
      description:
        "Remove a previously saved memory by its id. Memory ids are shown in brackets in the durable-memory block in your context.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          id: {
            type: "string",
            description: "The memory id to remove, e.g. mem_1a2b3c4d.",
          },
        },
        required: ["id"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const id = typeof args.id === "string" ? args.id.trim() : "";
        if (id.length === 0) {
          return { ok: false, error: "id is required" };
        }
        const handle = memoryHandle(execution.context);
        const store = await readMemory(handle);
        const before = store.entries.length;
        store.entries = store.entries.filter((entry) => entry.id !== id);
        const removed = before - store.entries.length;
        if (removed > 0) {
          await writeMemory(handle, store);
        }
        return { ok: removed > 0, id, removed };
      },
    },
  };
}

export function createMemoryToolInstances(): ToolInstance[] {
  return [rememberTool(), forgetTool()];
}

export function registerMemoryTools(registry: HarnessToolRegistry): void {
  for (const tool of createMemoryToolInstances()) {
    registry.register(tool);
  }
}

// Build the developer message that injects saved memory into the prompt.
// Returns null when nothing has been remembered yet.
export async function memoryInstruction(
  context: TurnContext,
): Promise<Message | null> {
  let store: MemoryStore;
  try {
    store = await readMemory(memoryHandle(context));
  } catch (error) {
    // Prompt assembly runs every model round, so a corrupt store must not brick
    // the agent in every conversation. Degrade the read: log loudly and tell the
    // model memory is unavailable rather than silently pretending it is empty.
    // The write path (remember/forget) still throws, so nothing overwrites the
    // corrupt artifact while it is broken.
    const detail = error instanceof Error ? error.message : String(error);
    console.error(`memory unavailable during prompt assembly: ${detail}`);
    return {
      role: "developer",
      content:
        "Your durable memory could not be read this turn (the stored memory artifact appears corrupt). Do not assume it is empty; if the user asks about saved facts, tell them memory is temporarily unavailable.",
    };
  }
  if (store.entries.length === 0) {
    return null;
  }
  const lines = store.entries.map((entry) => `- [${entry.id}] ${entry.text}`);
  return {
    role: "developer",
    content: `Durable memory you saved earlier with the remember tool. Treat these as authoritative context. Add new facts with remember and drop stale ones with forget(id).\n\n${lines.join("\n")}`,
  };
}

async function readLatestMemoryArtifact(
  handle: MemoryHandle,
): Promise<unknown | null> {
  const latest = latestArtifactVersion(
    await handle.listArtifacts(),
    MEMORY_ARTIFACT_PATH,
  );
  if (latest === null) {
    return null;
  }
  return handle.readArtifactJson({
    artifactId: latest.artifactId,
    version: latest.version,
  });
}

function latestArtifactVersion(
  artifacts: ArtifactVersion[],
  path: string,
): ArtifactVersion | null {
  return (
    artifacts
      .filter((artifact) => artifact.path === path)
      .sort((a, b) => b.version - a.version)[0] ?? null
  );
}

function isMemoryStore(value: unknown): value is MemoryStore {
  if (!isRecord(value) || !Array.isArray(value.entries)) {
    return false;
  }
  return value.entries.every(
    (entry) =>
      isRecord(entry) &&
      typeof entry.id === "string" &&
      typeof entry.text === "string" &&
      typeof entry.createdAt === "string",
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
