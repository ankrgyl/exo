import { describe, expect, it } from "vitest";

import {
  COMPACTION_CHECKPOINT_EVENT,
  COMPACTION_FAILED_EVENT,
  DEFAULT_COMPACTION_POLICY,
  checkpointToPayload,
  runCompaction,
  type CompactionPolicy,
  type SummarizeFn,
} from "./compaction";
import type { ArtifactVersion, Event, EventData, EventQuery } from "./index";

// --- stubs -------------------------------------------------------------------

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

// Turns carry realistic bulk: compaction deliberately does nothing when the
// compactable span is already smaller than the summary cap *plus* the envelope
// that wraps it into a prompt, so a fixture of single-character turns would
// exercise only that skip path. Sized well clear of the 8k default cap rather
// than a hair over it, so the guard's exact threshold is not load-bearing here
// — the test that pins that threshold sets its own cap.
const TURN_PADDING = "x".repeat(8_000);

function turn(text: string): Event[] {
  return [
    event("messages", {
      messages: [{ role: "user", content: `${text} ${TURN_PADDING}` }],
    }),
    event("turn_ended"),
  ];
}

class StubTarget {
  readonly record = { id: "conv-1", slug: "conv-1", name: "conv" };
  readonly appended: EventData[] = [];
  readonly written: Array<{ path: string; text: string }> = [];
  readonly events: Event[];
  private readonly artifacts = new Map<string, string>();
  private artifactSeq = 0;
  failArtifactWrite = false;
  /**
   * Makes `readArtifactText` throw rather than return null. The two are
   * different failures — a vanished artifact and a store that will not answer —
   * and the write path has to survive both.
   */
  failArtifactReads = false;

  constructor(events: Event[]) {
    this.events = events;
  }

  async getEvents(query?: EventQuery) {
    let events = [...this.events];
    if (query?.types) {
      const types = new Set(query.types);
      // A custom event is queryable by its `event_type`, not by the literal
      // "custom" — mirroring `EventData::kind()` in the Rust harness.
      events = events.filter((e) =>
        types.has(
          e.data.type === "custom" ? String(e.data.event_type) : e.data.type,
        ),
      );
    }
    if (query?.direction === "desc") {
      events.reverse();
    } else if (query?.cursor) {
      events = events.filter((e) => e.id > query.cursor!);
    }
    if (query?.limit != null) events = events.slice(0, query.limit);
    return { events, cursor: events.at(-1)?.id };
  }

  async readArtifactText(args: { artifactId: string }): Promise<string | null> {
    if (this.failArtifactReads) {
      throw new Error("artifact store unavailable");
    }
    return this.artifacts.get(args.artifactId) ?? null;
  }

  async writeArtifactText(args: {
    path: string;
    text: string;
  }): Promise<ArtifactVersion> {
    if (this.failArtifactWrite) {
      throw new Error("artifact store unavailable");
    }
    this.artifactSeq += 1;
    const id = artifactId(this.artifactSeq);
    this.artifacts.set(id, args.text);
    this.written.push(args);
    return {
      artifactId: id,
      path: args.path,
      version: 1,
      createdAt: new Date(0).toISOString(),
      sizeBytes: args.text.length,
    };
  }

  // `Turn.addEvents` takes the array directly; only `Conversation.addEvents`
  // wraps it in a request object.
  async addEvents(data: EventData[]) {
    this.appended.push(...data);
    for (const entry of data) {
      this.events.push(
        event(String(entry.type), entry as Record<string, unknown>),
      );
    }
    return { latestEventId: this.events.at(-1)!.id };
  }
}

function target(events: Event[]) {
  return new StubTarget(events) as unknown as Parameters<
    typeof runCompaction
  >[0]["conversation"] &
    StubTarget;
}

const policy: CompactionPolicy = {
  ...DEFAULT_COMPACTION_POLICY,
  keepRecentTurns: 1,
};

const summarize: SummarizeFn = async () => "SUMMARY OF EVERYTHING";

function args(stub: StubTarget, overrides: Record<string, unknown> = {}) {
  return {
    conversation: stub as never,
    turn: stub as never,
    policy,
    model: "test-model",
    agentModel: "test-model",
    promptTokensBefore: 123,
    summarize,
    ...overrides,
  } as Parameters<typeof runCompaction>[0];
}

/**
 * Checkpoints as they were actually appended. Matching on the custom-event
 * envelope rather than a flattened `type` is the point: a writer that emits
 * `{type: "exo.compaction.v1", ...}` is rejected by the Rust harness, so a
 * helper that accepted that shape would hide the failure it exists to catch.
 */
function checkpointEvents(stub: StubTarget): EventData[] {
  return stub.appended.filter(
    (d) => d.type === "custom" && d.event_type === COMPACTION_CHECKPOINT_EVENT,
  );
}

/** The decoded payload of a checkpoint event. */
function checkpointPayload(data: EventData): Record<string, unknown> {
  return data.payload as Record<string, unknown>;
}

// --- tests -------------------------------------------------------------------

describe("runCompaction", () => {
  it("writes a summary artifact and a checkpoint pointing at it", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    const result = await runCompaction(args(stub));

    expect(result.status).toBe("compacted");
    expect(stub.written).toHaveLength(1);
    expect(stub.written[0].text).toBe("SUMMARY OF EVERYTHING");

    const checkpoints = checkpointEvents(stub);
    expect(checkpoints).toHaveLength(1);
    const payload = checkpointPayload(checkpoints[0]);
    expect(payload.artifact_id).toBe(artifactId(1));
    expect(payload.artifact_path).toBe(stub.written[0].path);
    expect(payload.model).toBe("test-model");
    expect(payload.prompt_tokens_before).toBe(123);
  });

  it("does nothing when the conversation is too short to cut safely", async () => {
    const stub = target(turn("only"));
    const result = await runCompaction(args(stub));

    expect(result.status).toBe("skipped");
    expect(stub.appended).toHaveLength(0);
    expect(stub.written).toHaveLength(0);
  });

  it("feeds the previous summary into the next compaction", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    await runCompaction(args(stub));

    stub.appended.length = 0;
    const seen: string[] = [];
    stub.events.push(...turn("d"), ...turn("e"));
    await runCompaction(
      args(stub, {
        summarize: (async (input) => {
          seen.push(input.previousSummary ?? "");
          return "MERGED SUMMARY";
        }) satisfies SummarizeFn,
      }),
    );

    // Chained compaction must merge, not restart: dropping the prior summary
    // would silently lose everything before the first checkpoint.
    expect(seen[0]).toBe("SUMMARY OF EVERYTHING");
    const checkpoints = checkpointEvents(stub);
    const payload = checkpointPayload(checkpoints[0]);
    expect(payload.previous_checkpoint_id).not.toBeNull();
    // The count is what the agent is shown to judge how much history it is
    // missing, so it has to cover the whole chain, not just this pass.
    expect(Number(payload.compacted_event_count)).toBeGreaterThan(4);
  });

  it("stands down when a newer checkpoint lands mid-summary", async () => {
    // Turns on one conversation are not serialized, and the summarizer call is
    // the slowest step in a compaction. Everything in the checkpoint payload —
    // the chain link, the cumulative count, the cut boundary — is computed
    // against the head as it stood when the pass started. Readers take the
    // newest checkpoint, so publishing a stale one makes a shorter prefix
    // silently replace a longer one.
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    const result = await runCompaction(
      args(stub, {
        summarize: (async () => {
          const other = await runCompaction(
            args(stub, {
              summarize: (async () => "WINNER") satisfies SummarizeFn,
            }),
          );
          expect(other.status).toBe("compacted");
          return "LOSER";
        }) satisfies SummarizeFn,
      }),
    );

    expect(result.status).toBe("skipped");
    const checkpoints = checkpointEvents(stub);
    expect(checkpoints).toHaveLength(1);
    const payload = checkpointPayload(checkpoints[0]);
    // The surviving checkpoint points at the winner's artifact, not the
    // loser's — which was written before the race was noticed.
    expect(payload.artifact_id).toBe(artifactId(1));
  });

  it("rebuilds from the log when the prior summary read rejects", async () => {
    // Distinct from a missing artifact: the store errors. Letting that escape
    // fails the whole compaction, and every later attempt hits the same
    // artifact and fails identically — so an oversized conversation could never
    // publish a replacement checkpoint. Falling back costs one larger
    // summarizer call and loses nothing.
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    await runCompaction(args(stub));
    expect(checkpointEvents(stub)).toHaveLength(1);

    stub.appended.length = 0;
    stub.events.push(...turn("d"), ...turn("e"));
    stub.failArtifactReads = true;
    const seen: (string | null)[] = [];
    const result = await runCompaction(
      args(stub, {
        summarize: (async (input) => {
          seen.push(input.previousSummary);
          return "REBUILT SUMMARY";
        }) satisfies SummarizeFn,
      }),
    );

    expect(result.status).toBe("compacted");
    // No previous summary to merge, so the pass rebuilt from the start.
    expect(seen[0]).toBeNull();
    const payload = checkpointPayload(checkpointEvents(stub)[0]);
    expect(payload.previous_checkpoint_id).toBeNull();
  });

  it("caps an oversized summary rather than trusting the model", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    await runCompaction(
      args(stub, {
        policy: { ...policy, maxSummaryChars: 50 },
        summarize: (async () => "x".repeat(5_000)) satisfies SummarizeFn,
      }),
    );
    expect(stub.written[0].text.length).toBeLessThanOrEqual(50);
  });

  it("never fails the turn when the summarizer throws", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    const result = await runCompaction(
      args(stub, {
        summarize: (async () => {
          throw new Error("model unavailable");
        }) satisfies SummarizeFn,
      }),
    );

    expect(result.status).toBe("failed");
    expect(checkpointEvents(stub)).toHaveLength(0);
    // The failure is recorded so the agent can see why its prompt is still big.
    const failures = stub.appended.filter(
      (d) => d.type === "custom" && d.event_type === COMPACTION_FAILED_EVENT,
    );
    expect(failures).toHaveLength(1);
    expect(String(checkpointPayload(failures[0]).error)).toContain(
      "model unavailable",
    );
  });

  it("never fails the turn when the artifact write throws", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    stub.failArtifactWrite = true;
    const result = await runCompaction(args(stub));

    expect(result.status).toBe("failed");
    expect(checkpointEvents(stub)).toHaveLength(0);
  });

  it("routes a rebuild from the start of the log through the agent model", async () => {
    // The summary model is chosen against the materialized prompt — summary
    // plus retained tail — because that is the only size available before a cut
    // point exists. A broken previous checkpoint forces a rebuild from the
    // whole history, which can be far larger, so a cheaper model that fit the
    // prompt may not fit this and the repair would be rejected while the
    // agent's model had room.
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    await runCompaction(args(stub, { model: "cheap", agentModel: "agent" }));

    // Break the artifact so the next pass has to rebuild.
    stub.failArtifactReads = true;
    stub.appended.length = 0;
    stub.events.push(...turn("d"), ...turn("e"));

    // The model the summarizer is *asked* for, not just the one recorded:
    // asserting only on the checkpoint would pass while the request still went
    // to the cheaper model, and a checkpoint naming a model that never saw the
    // span is worse than the bug it claims to have fixed.
    let requested: string | null = null;
    const result = await runCompaction(
      args(stub, {
        model: "cheap",
        agentModel: "agent",
        summarize: (async (input) => {
          requested = input.model;
          return "REBUILT SUMMARY";
        }) satisfies SummarizeFn,
      }),
    );

    expect(result.status).toBe("compacted");
    expect(requested).toBe("agent");
    expect(checkpointPayload(checkpointEvents(stub)[0]).model).toBe("agent");
  });

  it("writes no checkpoint when the summary comes back empty", async () => {
    // An empty summary would checkpoint away real history and replace it with
    // nothing — strictly worse than not compacting.
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    const result = await runCompaction(
      args(stub, { summarize: (async () => "   ") satisfies SummarizeFn }),
    );
    expect(result.status).toBe("failed");
    expect(checkpointEvents(stub)).toHaveLength(0);
  });

  it("writes no checkpoint when the summary comes back larger than the span", async () => {
    // The pre-check has to guess the summary's size, and it guesses by pricing
    // the character cap at the *span's* bytes-per-character. That is a fair
    // heuristic — a summary is usually written in the script it summarizes —
    // but only a heuristic: a summary that reaches for another script is 4
    // bytes per character where the span was 1. Publishing it would enlarge the
    // very prompt compaction was invoked to shrink, and the checkpoint would
    // persist that until the next cut.
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    // Compliant on characters, four bytes each.
    const bloated = "😀".repeat(policy.maxSummaryChars);
    const result = await runCompaction(
      args(stub, { summarize: (async () => bloated) satisfies SummarizeFn }),
    );

    expect(result.status).toBe("skipped");
    expect(checkpointEvents(stub)).toHaveLength(0);
  });

  it("writes no checkpoint when the summary shrinks bytes but grows tokens", async () => {
    // Bytes and tokens do not move together, and the context window is
    // denominated in tokens. 3 turns of ~8KB ASCII is ~24KB and ~8k estimated
    // tokens; 5000 emoji is 20KB — a clear win on bytes — but ~10k tokens, so
    // it takes *more* of the window than the history it replaces. Only the
    // token clause catches this; the byte comparison alone would publish it.
    const stub = target([
      ...turn("a"),
      ...turn("b"),
      ...turn("c"),
      ...turn("d"),
    ]);
    const dense = "\u{1F600}".repeat(5_000);
    const result = await runCompaction(
      args(stub, { summarize: (async () => dense) satisfies SummarizeFn }),
    );

    expect(result.status).toBe("skipped");
    expect(checkpointEvents(stub)).toHaveLength(0);
  });

  it("skips when the compactable span is smaller than the summary cap", async () => {
    // A prompt can cross the threshold because of the *retained* turns — one
    // huge tool result, say. Summarizing a tiny prefix into an 8k-character
    // summary would grow the prompt, not shrink it, and cost a model call to do
    // it. Nothing to reclaim means nothing to do.
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    let called = false;
    const result = await runCompaction(
      args(stub, {
        policy: { ...policy, maxSummaryChars: 1_000_000 },
        summarize: (async () => {
          called = true;
          return "SUMMARY";
        }) satisfies SummarizeFn,
      }),
    );
    expect(result.status).toBe("skipped");
    expect(called).toBe(false);
    expect(stub.written).toHaveLength(0);
  });

  it("summarizes only the events being compacted", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    let received: string[] = [];
    await runCompaction(
      args(stub, {
        summarize: (async (input) => {
          received = input.messages.map((m) => String(m.content).split(" ")[0]);
          return "SUMMARY";
        }) satisfies SummarizeFn,
      }),
    );
    // `c` is the kept turn; folding it into the summary would duplicate it in
    // the next prompt, once as summary and once verbatim.
    expect(received).toEqual(["a", "b"]);
  });
});

describe("checkpoint payload shape", () => {
  it("matches what the read path decodes", () => {
    const payload = checkpointToPayload({
      upToEventId: eventId(1),
      artifactId: artifactId(1),
      artifactPath: "compaction/conv-1/1.md",
      artifactVersion: 1,
      previousCheckpointId: null,
      compactedEventCount: 4,
      summaryChars: 20,
      promptTokensBefore: 100,
      model: "m",
    });
    expect(Object.keys(payload).sort()).toEqual(
      [
        "artifact_id",
        "artifact_path",
        "artifact_version",
        "compacted_event_count",
        "model",
        "previous_checkpoint_id",
        "prompt_tokens_before",
        "summary_chars",
        "up_to_event_id",
      ].sort(),
    );
  });
});
