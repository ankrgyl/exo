import { describe, expect, it } from "vitest";

import { COMPACTION_CHECKPOINT_EVENT, checkpointToPayload } from "./compaction";
import {
  PromptHistoryCache,
  materializeConversationMessages,
  materializePromptMessages,
  readActiveCheckpoint,
  type ArtifactVersion,
  type Conversation,
  type Event,
  type EventQuery,
  type GetEventsResult,
  type Message,
} from "./index";

// --- a Conversation stub that records how it was queried ---------------------
//
// Recording the queries matters as much as the messages: compaction is only
// worth anything if the read path actually narrows the event scan, and a stub
// that ignores `cursor` would let a broken implementation pass.

interface StubOptions {
  events: Event[];
  artifacts?: Map<string, string>;
}

class StubConversation {
  readonly queries: EventQuery[] = [];
  readonly artifactReads: string[] = [];
  private readonly events: Event[];
  private readonly artifacts: Map<string, string>;

  /**
   * Makes `readArtifactText` throw rather than return null. The two are
   * different failures — a vanished artifact and a store that will not answer —
   * and the read path has to survive both.
   */
  failArtifactReads = false;

  constructor(options: StubOptions) {
    this.events = options.events;
    this.artifacts = options.artifacts ?? new Map();
  }

  append(...events: Event[]): void {
    this.events.push(...events);
  }

  async getEvents(query?: EventQuery): Promise<GetEventsResult> {
    this.queries.push(query ?? {});
    let events = [...this.events];
    if (query?.types) {
      const types = new Set(query.types);
      events = events.filter((event) => types.has(eventKind(event)));
    }
    if (query?.direction === "desc") {
      events.reverse();
      if (query.cursor) {
        events = events.filter((event) => event.id < query.cursor!);
      }
    } else if (query?.cursor) {
      // Matches the exoharness contract: the cursor is exclusive.
      events = events.filter((event) => event.id > query.cursor!);
    }
    if (query?.limit != null) {
      events = events.slice(0, query.limit);
    }
    return { events, cursor: events.at(-1)?.id };
  }

  async readArtifactText(args: {
    artifactId: string;
    version?: number;
  }): Promise<string | null> {
    this.artifactReads.push(args.artifactId);
    if (this.failArtifactReads) {
      throw new Error("artifact store unavailable");
    }
    return this.artifacts.get(args.artifactId) ?? null;
  }

  async listArtifacts(): Promise<ArtifactVersion[]> {
    throw new Error(
      "listArtifacts must not be called: the checkpoint carries the artifact id",
    );
  }
}

function asConversation(stub: StubConversation): Conversation {
  return stub as unknown as Conversation;
}

/**
 * The kind an event is queryable by. A custom event's kind is its `event_type`,
 * not the literal `"custom"` — mirroring `EventData::kind()` in
 * `crates/exoharness/src/types.rs`. Filtering on `data.type` instead would make
 * the stub disagree with the real harness and let a broken read path pass.
 */
function eventKind(event: Event): string {
  return event.data.type === "custom"
    ? String(event.data.event_type)
    : event.data.type;
}

// --- event builders ----------------------------------------------------------

// Event ids must be syntactically valid UUIDs: the decoder rejects malformed
// ones because Rust does, and a fake like eventId(1) would exercise a laxer
// contract than production. Zero-padded counters keep them sorting ascending
// the way UUIDv7 does.
function eventId(n: number): string {
  return `01920000-0000-7000-8000-${String(n).padStart(12, "0")}`;
}

/** Artifact ids are uuids too; a distinct prefix keeps them apart from events. */
function artifactId(n: number): string {
  return `01920000-0000-7000-9000-${String(n).padStart(12, "0")}`;
}

let nextId = 0;
function event(type: string, extra: Record<string, unknown> = {}): Event {
  nextId += 1;
  return {
    id: eventId(nextId),
    conversationId: "conv-1",
    createdAt: new Date(0).toISOString(),
    data: { type, ...extra },
  };
}

function userMessage(text: string): Event {
  return event("messages", { messages: [{ role: "user", content: text }] });
}

function turn(text: string): Event[] {
  return [userMessage(text), event("turn_ended")];
}

function checkpointEvent(args: {
  upToEventId: string;
  artifactId: string;
}): Event {
  // The real custom-event envelope: `{type: "custom", event_type, payload}`.
  // Building a flattened event here would make these tests agree with a writer
  // that the Rust harness rejects.
  return event("custom", {
    event_type: COMPACTION_CHECKPOINT_EVENT,
    payload: checkpointToPayload({
      upToEventId: args.upToEventId,
      artifactId: args.artifactId,
      artifactPath: "compaction/conv-1/1.md",
      artifactVersion: 1,
      previousCheckpointId: null,
      compactedEventCount: 4,
      summaryChars: 20,
      promptTokensBefore: 100,
      model: "test-model",
    }),
  });
}

function texts(messages: Message[]): string[] {
  return messages.map((message) =>
    typeof message.content === "string"
      ? message.content
      : JSON.stringify(message.content),
  );
}

// --- tests -------------------------------------------------------------------

describe("materializeConversationMessages without a checkpoint", () => {
  it("returns the full history, exactly as before", async () => {
    const events = [...turn("one"), ...turn("two")];
    const stub = new StubConversation({ events });
    const messages = await materializeConversationMessages(
      asConversation(stub),
    );
    expect(texts(messages)).toEqual(["one", "two"]);
  });
});

describe("materializeConversationMessages with a checkpoint", () => {
  it("replaces compacted history with the summary and keeps the tail", async () => {
    const older = [...turn("ancient"), ...turn("old")];
    const cut = older.at(-1)!;
    const checkpoint = checkpointEvent({
      upToEventId: cut.id,
      artifactId: artifactId(1),
    });
    const recent = turn("recent");
    const stub = new StubConversation({
      events: [...older, checkpoint, ...recent],
      artifacts: new Map([[artifactId(1), "SUMMARY: the user likes tea"]]),
    });

    const messages = await materializeConversationMessages(
      asConversation(stub),
    );
    const rendered = texts(messages);

    expect(
      rendered.some((t) => t.includes("SUMMARY: the user likes tea")),
    ).toBe(true);
    expect(rendered).toContain("recent");
    expect(rendered).not.toContain("ancient");
    expect(rendered).not.toContain("old");
  });

  it("puts the summary before the retained tail", async () => {
    const older = turn("ancient");
    const checkpoint = checkpointEvent({
      upToEventId: older.at(-1)!.id,
      artifactId: artifactId(1),
    });
    const stub = new StubConversation({
      events: [...older, checkpoint, ...turn("recent")],
      artifacts: new Map([[artifactId(1), "SUMMARY"]]),
    });

    const rendered = texts(
      await materializeConversationMessages(asConversation(stub)),
    );
    const summaryIndex = rendered.findIndex((t) => t.includes("SUMMARY"));
    const tailIndex = rendered.indexOf("recent");
    expect(summaryIndex).toBeGreaterThanOrEqual(0);
    expect(summaryIndex).toBeLessThan(tailIndex);
  });

  it("scans only events after the checkpoint", async () => {
    const older = [...turn("a"), ...turn("b"), ...turn("c")];
    const checkpoint = checkpointEvent({
      upToEventId: older.at(-1)!.id,
      artifactId: artifactId(1),
    });
    const stub = new StubConversation({
      events: [...older, checkpoint, ...turn("recent")],
      artifacts: new Map([[artifactId(1), "SUMMARY"]]),
    });

    await materializeConversationMessages(asConversation(stub));

    // The history scan must carry the checkpoint's cursor. Without it the
    // prompt would shrink but the read would still be O(whole log).
    const historyQuery = stub.queries.find((q) =>
      q.types?.includes("messages"),
    );
    expect(historyQuery?.cursor).toBe(older.at(-1)!.id);
  });

  it("resolves the summary by artifact id, not by listing artifacts", async () => {
    const older = turn("ancient");
    const stub = new StubConversation({
      events: [
        ...older,
        checkpointEvent({
          upToEventId: older.at(-1)!.id,
          artifactId: artifactId(7),
        }),
        ...turn("recent"),
      ],
      artifacts: new Map([[artifactId(7), "SUMMARY"]]),
    });

    // listArtifacts() throws in the stub; reaching it is the failure.
    await materializeConversationMessages(asConversation(stub));
    expect(stub.artifactReads).toEqual([artifactId(7)]);
  });

  it("falls back to full history when the summary artifact is missing", async () => {
    // A checkpoint pointing at a vanished artifact must not silently erase
    // history — better a large prompt than a prompt with a hole in it.
    const older = turn("ancient");
    const stub = new StubConversation({
      events: [
        ...older,
        checkpointEvent({
          upToEventId: older.at(-1)!.id,
          artifactId: artifactId(999),
        }),
        ...turn("recent"),
      ],
      artifacts: new Map(),
    });

    const rendered = texts(
      await materializeConversationMessages(asConversation(stub)),
    );
    expect(rendered).toContain("ancient");
    expect(rendered).toContain("recent");
  });

  it("falls back to full history when the artifact store refuses the read", async () => {
    // Distinct from a missing artifact: the store errors. The raw log is
    // equally intact either way, so failing here would take a working
    // conversation down — and keep taking it down, since every later turn
    // consults the same checkpoint. Same handling as the Rust executor's
    // `read_summary_or_fall_back`.
    const older = turn("ancient");
    const stub = new StubConversation({
      events: [
        ...older,
        checkpointEvent({
          upToEventId: older.at(-1)!.id,
          artifactId: artifactId(1),
        }),
        ...turn("recent"),
      ],
      artifacts: new Map([[artifactId(1), "SUMMARY OF EARLIER"]]),
    });
    stub.failArtifactReads = true;

    const rendered = texts(
      await materializeConversationMessages(asConversation(stub)),
    );
    expect(rendered).toContain("ancient");
    expect(rendered).toContain("recent");
    expect(rendered).not.toContain("SUMMARY OF EARLIER");
  });

  it("uses the newest checkpoint when several exist", async () => {
    const first = turn("ancient");
    const firstCheckpoint = checkpointEvent({
      upToEventId: first.at(-1)!.id,
      artifactId: artifactId(1),
    });
    const middle = turn("middle");
    const secondCheckpoint = checkpointEvent({
      upToEventId: middle.at(-1)!.id,
      artifactId: artifactId(2),
    });
    const stub = new StubConversation({
      events: [
        ...first,
        firstCheckpoint,
        ...middle,
        secondCheckpoint,
        ...turn("recent"),
      ],
      artifacts: new Map([
        [artifactId(1), "OLD SUMMARY"],
        [artifactId(2), "NEW SUMMARY"],
      ]),
    });

    const rendered = texts(
      await materializeConversationMessages(asConversation(stub)),
    );
    expect(rendered.some((t) => t.includes("NEW SUMMARY"))).toBe(true);
    expect(rendered.some((t) => t.includes("OLD SUMMARY"))).toBe(false);
    expect(rendered).not.toContain("middle");
    expect(rendered).toContain("recent");
  });
});

describe("readActiveCheckpoint", () => {
  it("returns null when the conversation has never been compacted", async () => {
    const stub = new StubConversation({ events: turn("one") });
    expect(await readActiveCheckpoint(asConversation(stub))).toBeNull();
  });

  it("returns the newest checkpoint", async () => {
    const older = turn("ancient");
    const checkpoint = checkpointEvent({
      upToEventId: older.at(-1)!.id,
      artifactId: artifactId(1),
    });
    const stub = new StubConversation({
      events: [...older, checkpoint, ...turn("recent")],
    });
    const active = await readActiveCheckpoint(asConversation(stub));
    expect(active?.artifactId).toBe(artifactId(1));
  });
});

describe("PromptHistoryCache", () => {
  it("re-fetches the whole log only once, then reads incrementally", async () => {
    const events = [...turn("one"), ...turn("two")];
    const stub = new StubConversation({ events });
    const cache = new PromptHistoryCache();

    await cache.materialize(asConversation(stub));
    const afterFirst = stub.queries.length;
    await cache.materialize(asConversation(stub));
    await cache.materialize(asConversation(stub));

    const historyQueries = stub.queries.filter((q) =>
      q.types?.includes("messages"),
    );
    // The priming read has no cursor; every later one must, or the cache is
    // not actually saving the scan it exists to save.
    expect(historyQueries[0].cursor ?? null).toBeNull();
    expect(historyQueries.slice(1).every((q) => q.cursor != null)).toBe(true);
    expect(stub.queries.length).toBeGreaterThan(afterFirst);
  });

  it("produces the same messages as an uncached materialization", async () => {
    const events = [...turn("one"), ...turn("two")];
    const cached = new PromptHistoryCache();
    const cachedMessages = await cached.materialize(
      asConversation(new StubConversation({ events })),
    );
    const direct = await materializeConversationMessages(
      asConversation(new StubConversation({ events })),
    );
    expect(cachedMessages).toEqual(direct);
  });

  it("picks up events appended between rounds", async () => {
    const events = [...turn("one")];
    const stub = new StubConversation({ events });
    const cache = new PromptHistoryCache();

    expect(texts(await cache.materialize(asConversation(stub)))).toEqual([
      "one",
    ]);
    events.push(...turn("two"));
    expect(texts(await cache.materialize(asConversation(stub)))).toEqual([
      "one",
      "two",
    ]);
  });

  it("rebuilds from the checkpoint after invalidation", async () => {
    // Compaction replaces exactly the prefix the cache is holding, so a stale
    // cache would silently resurrect the history that was just compacted away.
    const older = turn("ancient");
    const stub = new StubConversation({
      events: [...older],
      artifacts: new Map([[artifactId(1), "SUMMARY"]]),
    });
    const cache = new PromptHistoryCache();
    expect(texts(await cache.materialize(asConversation(stub)))).toEqual([
      "ancient",
    ]);

    stub.append(
      checkpointEvent({
        upToEventId: older.at(-1)!.id,
        artifactId: artifactId(1),
      }),
    );
    stub.append(...turn("recent"));

    cache.invalidate();
    const rendered = texts(await cache.materialize(asConversation(stub)));
    expect(rendered.some((t) => t.includes("SUMMARY"))).toBe(true);
    expect(rendered).not.toContain("ancient");
    expect(rendered).toContain("recent");
  });

  it("notices a checkpoint written by another turn, without invalidation", async () => {
    // `invalidate()` only reaches the cache belonging to the turn that
    // compacted. Turns on one conversation are not serialized, so a cache primed
    // before someone else's compaction must still notice it — otherwise this
    // turn extends from its old cursor, never sees the checkpoint, and replays
    // the compacted prefix for the rest of its tool rounds.
    const older = turn("ancient");
    const stub = new StubConversation({
      events: [...older],
      artifacts: new Map([[artifactId(1), "SUMMARY"]]),
    });
    const cache = new PromptHistoryCache();
    expect(texts(await cache.materialize(asConversation(stub)))).toEqual([
      "ancient",
    ]);

    // Another turn compacts. This cache is never told.
    stub.append(
      checkpointEvent({
        upToEventId: older.at(-1)!.id,
        artifactId: artifactId(1),
      }),
    );
    stub.append(...turn("recent"));

    const rendered = texts(await cache.materialize(asConversation(stub)));
    expect(rendered.some((t) => t.includes("SUMMARY"))).toBe(true);
    expect(rendered).not.toContain("ancient");
    expect(rendered).toContain("recent");
  });

  it("keeps a tool round intact when its result lands in a later round", async () => {
    // The incremental path must not treat a batch boundary as a turn boundary:
    // a request fetched in one round and its result in the next still has to
    // materialize as a completed call, not a fabricated failure.
    const stub = new StubConversation({ events: [] });
    stub.append(
      event("messages", {
        messages: [
          {
            role: "assistant",
            content: [
              {
                type: "tool_call",
                tool_call_id: "call_1",
                tool_name: "shell",
                arguments: {},
              },
            ],
          },
        ],
      }),
      event("tool_requested", {
        tool_call_id: "call_1",
        request: { function_name: "shell", arguments: {} },
      }),
    );
    const cache = new PromptHistoryCache();
    await cache.materialize(asConversation(stub));

    stub.append(
      event("tool_result", { tool_call_id: "call_1", result: { ok: true } }),
    );
    const messages = await cache.materialize(asConversation(stub));

    const rendered = JSON.stringify(messages);
    expect(rendered).not.toContain("did not complete");
    expect(rendered).toContain('"ok":true');
  });
});

describe("materializePromptMessages", () => {
  it("keeps instructions ahead of the summary and history", async () => {
    const older = turn("ancient");
    const stub = new StubConversation({
      events: [
        ...older,
        checkpointEvent({
          upToEventId: older.at(-1)!.id,
          artifactId: artifactId(1),
        }),
        ...turn("recent"),
      ],
      artifacts: new Map([[artifactId(1), "SUMMARY"]]),
    });

    const rendered = texts(
      await materializePromptMessages(asConversation(stub), [
        { role: "developer", content: "INSTRUCTIONS" },
      ]),
    );
    expect(rendered[0]).toBe("INSTRUCTIONS");
    expect(rendered[1]).toContain("SUMMARY");
    expect(rendered.at(-1)).toBe("recent");
  });
});
