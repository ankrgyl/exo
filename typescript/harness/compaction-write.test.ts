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

let nextId = 0;
function event(type: string, extra: Record<string, unknown> = {}): Event {
  nextId += 1;
  return {
    id: `evt-${String(nextId).padStart(6, "0")}`,
    conversationId: "conv-1",
    createdAt: new Date(0).toISOString(),
    data: { type, ...extra },
  };
}

function turn(text: string): Event[] {
  return [
    event("messages", { messages: [{ role: "user", content: text }] }),
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

  constructor(events: Event[]) {
    this.events = events;
  }

  async getEvents(query?: EventQuery) {
    let events = [...this.events];
    if (query?.types) {
      const types = new Set(query.types);
      events = events.filter((e) => types.has(e.data.type));
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
    const artifactId = `art-${this.artifactSeq}`;
    this.artifacts.set(artifactId, args.text);
    this.written.push(args);
    return {
      artifactId,
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
    promptTokensBefore: 123,
    summarize,
    ...overrides,
  } as Parameters<typeof runCompaction>[0];
}

function checkpointEvents(stub: StubTarget): EventData[] {
  return stub.appended.filter((d) => d.type === COMPACTION_CHECKPOINT_EVENT);
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
    expect(checkpoints[0].artifact_id).toBe("art-1");
    expect(checkpoints[0].artifact_path).toBe(stub.written[0].path);
    expect(checkpoints[0].model).toBe("test-model");
    expect(checkpoints[0].prompt_tokens_before).toBe(123);
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
    expect(checkpoints[0].previous_checkpoint_id).not.toBeNull();
    // The count is what the agent is shown to judge how much history it is
    // missing, so it has to cover the whole chain, not just this pass.
    expect(Number(checkpoints[0].compacted_event_count)).toBeGreaterThan(4);
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
      (d) => d.type === COMPACTION_FAILED_EVENT,
    );
    expect(failures).toHaveLength(1);
    expect(String(failures[0].error)).toContain("model unavailable");
  });

  it("never fails the turn when the artifact write throws", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    stub.failArtifactWrite = true;
    const result = await runCompaction(args(stub));

    expect(result.status).toBe("failed");
    expect(checkpointEvents(stub)).toHaveLength(0);
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

  it("summarizes only the events being compacted", async () => {
    const stub = target([...turn("a"), ...turn("b"), ...turn("c")]);
    let received: string[] = [];
    await runCompaction(
      args(stub, {
        summarize: (async (input) => {
          received = input.messages.map((m) => String(m.content));
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
      upToEventId: "evt-1",
      artifactId: "art-1",
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
