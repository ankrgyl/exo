import { describe, expect, it } from "vitest";

import {
  COMPACTION_CHECKPOINT_EVENT,
  checkpointToPayload,
  createToolRegistry,
  materializePromptHistory,
  type Event,
  type EventQuery,
  type TurnContext,
} from "@exo/harness";

import {
  compactionInstruction,
  registerCompactionTools,
} from "./compaction-tools";

// --- fixture -----------------------------------------------------------------

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

/**
 * A fresh artifact id per checkpoint.
 *
 * Summary reads are memoized by `artifactId@version` so the notice and the
 * prompt cannot describe different contexts. Reusing one id across tests would
 * let a test that reads successfully prime the memo for a later test that means
 * to exercise the failure path.
 */
let nextArtifact = 0;

function checkpointEvent(): Event {
  nextArtifact += 1;
  // The real custom-event envelope; see tests/fixtures/README.md.
  return event("custom", {
    event_type: COMPACTION_CHECKPOINT_EVENT,
    payload: checkpointToPayload({
      upToEventId: eventId(1),
      artifactId: artifactId(nextArtifact),
      artifactPath: "compaction/conv-1/summary.md",
      artifactVersion: 2,
      previousCheckpointId: null,
      compactedEventCount: 40,
      summaryChars: 512,
      promptTokensBefore: 150_000,
      model: "summary-model",
    }),
  });
}

/** Ordinary retained history, so the fallback has something to fall back to. */
function turnEvents(text: string): Event[] {
  return [
    event("messages", { messages: [{ role: "user", content: text }] }),
    event("turn_ended"),
  ];
}

function makeContext(options: {
  events: Event[];
  summary?: string;
  compaction?: Record<string, unknown> | null;
  /** Makes the artifact read reject rather than return null. */
  failArtifactReads?: boolean;
  /**
   * Makes the artifact read succeed this many times, then reject — a store that
   * goes down between two reads of the same summary.
   */
  failArtifactReadsAfter?: number;
}): TurnContext {
  let artifactReads = 0;
  const conversation = {
    record: { id: "conv-1", slug: "conv-1", name: "conv" },
    async getEvents(query?: EventQuery) {
      let events = [...options.events];
      if (query?.types) {
        const types = new Set(query.types);
        // Custom events are queryable by `event_type`, mirroring the Rust
        // harness's `EventData::kind()`.
        events = events.filter((e) =>
          types.has(
            e.data.type === "custom" ? String(e.data.event_type) : e.data.type,
          ),
        );
      }
      if (query?.direction === "desc") events.reverse();
      if (query?.limit != null) events = events.slice(0, query.limit);
      return { events, cursor: events.at(-1)?.id };
    },
    async readArtifactText() {
      if (options.failArtifactReads) {
        throw new Error("artifact store unavailable");
      }
      if (options.failArtifactReadsAfter !== undefined) {
        if (artifactReads >= options.failArtifactReadsAfter) {
          throw new Error("artifact store unavailable");
        }
        artifactReads += 1;
      }
      return options.summary ?? null;
    },
  };
  const turn = {
    async addEvents() {
      return { latestEventId: "evt-x" };
    },
    // The registry spills oversized tool results to artifacts; these results
    // are small, but the registry still needs the method to exist.
    async writeArtifactText(args: { path: string }) {
      return {
        artifactId: artifactId(11),
        path: args.path,
        version: 1,
        createdAt: new Date(0).toISOString(),
        sizeBytes: 0,
      };
    },
  };
  return {
    agentConfig: { compaction: options.compaction ?? null },
    exoharness: { current: { conversation, turn } },
  } as unknown as TurnContext;
}

async function callTool(context: TurnContext, name: string) {
  const registry = createToolRegistry(context);
  registerCompactionTools(registry);
  const events = await registry.executePending([
    { toolCallId: "call-1", request: { functionName: name, arguments: {} } },
  ]);
  // The registry wraps every result in an envelope and puts the tool's own
  // payload under `value` (spilling it to an artifact when oversized).
  const envelope = events[0].result as Record<string, unknown>;
  return envelope.value as Record<string, unknown>;
}

// --- tests -------------------------------------------------------------------

describe("describe_compaction", () => {
  it("reports the effective policy when nothing is configured", async () => {
    const result = await callTool(
      makeContext({ events: [] }),
      "describe_compaction",
    );
    expect(result.ok).toBe(true);
    const policy = result.policy as Record<string, unknown>;
    expect(policy.enabled).toBe(true);
    expect(policy.thresholdRatio).toBe(0.7);
    expect(result.checkpoint).toBeNull();
  });

  it("reflects a configured override", async () => {
    const result = await callTool(
      makeContext({
        events: [],
        compaction: { enabled: false, keep_recent_turns: 9 },
      }),
      "describe_compaction",
    );
    const policy = result.policy as Record<string, unknown>;
    expect(policy.enabled).toBe(false);
    expect(policy.keepRecentTurns).toBe(9);
  });

  it("reports the active checkpoint so the agent can see what it lost", async () => {
    const result = await callTool(
      makeContext({ events: [checkpointEvent()], summary: "SUMMARY TEXT" }),
      "describe_compaction",
    );
    const checkpoint = result.checkpoint as Record<string, unknown>;
    expect(checkpoint.compactedEventCount).toBe(40);
    expect(checkpoint.summaryChars).toBe(512);
    expect(checkpoint.upToEventId).toBe(eventId(1));
  });
});

describe("read_compaction_summary", () => {
  it("returns the current summary text", async () => {
    const result = await callTool(
      makeContext({ events: [checkpointEvent()], summary: "SUMMARY TEXT" }),
      "read_compaction_summary",
    );
    expect(result.ok).toBe(true);
    expect(result.summary).toBe("SUMMARY TEXT");
  });

  it("says so plainly when nothing has been compacted", async () => {
    const result = await callTool(
      makeContext({ events: [] }),
      "read_compaction_summary",
    );
    expect(result.ok).toBe(true);
    expect(result.summary).toBeNull();
  });
});

describe("compactionInstruction", () => {
  it("is absent until the conversation has actually been compacted", async () => {
    // No point spending prompt space explaining compaction to an agent whose
    // history is still entirely intact.
    expect(await compactionInstruction(makeContext({ events: [] }))).toBeNull();
  });

  it("stays silent when the checkpoint's summary cannot be read", async () => {
    // Materialization falls back to the full log when the artifact is gone, so
    // the prompt contains no summary. Announcing one would describe a context
    // the agent does not have — it would hunt for detail already in front of
    // it, or tell the user history is missing when none is.
    expect(
      await compactionInstruction(
        makeContext({ events: [checkpointEvent()], summary: undefined }),
      ),
    ).toBeNull();
  });

  it("stays silent when the artifact store refuses the read", async () => {
    // This runs while instructions are assembled, before materialization gets
    // its chance to fall back — so letting the rejection escape would fail
    // every turn on a conversation whose raw log is perfectly usable, over a
    // notice that is decoration.
    expect(
      await compactionInstruction(
        makeContext({
          events: [checkpointEvent()],
          summary: "S",
          failArtifactReads: true,
        }),
      ),
    ).toBeNull();
  });

  it("tells the agent its history was compacted and how to recover detail", async () => {
    const message = await compactionInstruction(
      makeContext({ events: [checkpointEvent()], summary: "S" }),
    );
    expect(message).not.toBeNull();
    const content = String(message?.content);
    expect(content).toContain("40");
    expect(content).toContain("list_conversation_events");
  });

  it("never claims a summary the prompt built moments later does not contain", async () => {
    // Two independent reads of the same artifact can disagree: this one
    // succeeds, materialization's fails transiently, and the agent gets the
    // full raw log underneath a developer message insisting the older part was
    // replaced by a summary above it. That is the exact failure this notice was
    // added to prevent, reintroduced by reading twice.
    //
    // The order is the harmful one: instructions are assembled before the
    // prompt history, so the notice reads first and commits to the claim.
    const context = makeContext({
      events: [checkpointEvent(), ...turnEvents("recent")],
      summary: "SUMMARY OF EARLIER",
      failArtifactReadsAfter: 1,
    });

    const message = await compactionInstruction(context);
    expect(message).not.toBeNull();

    const rendered = (
      await materializePromptHistory(context.exoharness.current.conversation)
    )
      .map((m) => String(m.content))
      .join("\n");
    expect(rendered).toContain("SUMMARY OF EARLIER");
  });
});
