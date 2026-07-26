import { describe, expect, it } from "vitest";

import {
  COMPACTION_CHECKPOINT_EVENT,
  CompactionGate,
  DEFAULT_COMPACTION_POLICY,
  capSummary,
  checkpointFromEvent,
  estimatedTokensFromChars,
  resolveCompactionPolicy,
  selectCutPoint,
  shouldCompact,
  type CompactionCheckpoint,
} from "./compaction";
import type { Event } from "./index";

// --- event stream builders ---------------------------------------------------

let nextId = 0;

// Event ids only need to sort ascending the way UUIDv7 does; zero-padded
// counters give the same ordering with readable failures.
function event(type: string, extra: Record<string, unknown> = {}): Event {
  nextId += 1;
  return {
    id: `evt-${String(nextId).padStart(6, "0")}`,
    conversationId: "conv-1",
    createdAt: new Date(0).toISOString(),
    data: { type, ...extra },
  };
}

function messages(text: string): Event {
  return event("messages", {
    messages: [{ role: "assistant", content: text }],
  });
}

function toolPair(callId: string): Event[] {
  return [
    event("tool_requested", {
      tool_call_id: callId,
      request: { function_name: "shell", arguments: {} },
    }),
    event("tool_result", { tool_call_id: callId, result: { ok: true } }),
  ];
}

function turnEnded(): Event {
  return event("turn_ended");
}

/** A complete turn: a message, `toolRounds` tool pairs, then turn_ended. */
function turn(label: string, toolRounds: number): Event[] {
  const events: Event[] = [messages(`turn ${label}`)];
  for (let i = 0; i < toolRounds; i += 1) {
    events.push(...toolPair(`${label}-call-${i}`));
  }
  events.push(turnEnded());
  return events;
}

// --- the invariant under test ------------------------------------------------

/**
 * The correctness constraint compaction exists to protect. Cutting a stream so
 * that a tool_requested and its tool_result land on opposite sides corrupts the
 * prompt: a request without its result makes the materializer fabricate a
 * "tool execution did not complete" failure for a call that actually succeeded,
 * and a result without its request is silently dropped.
 */
function splitsAToolRound(events: Event[], upToEventId: string): boolean {
  const compacted = new Set<string>();
  const retained = new Set<string>();
  let seenCut = false;
  for (const e of events) {
    const callId = e.data.tool_call_id;
    if (typeof callId === "string") {
      (seenCut ? retained : compacted).add(callId);
    }
    if (e.id === upToEventId) {
      seenCut = true;
    }
  }
  for (const callId of retained) {
    if (compacted.has(callId)) return true;
  }
  return false;
}

// Deterministic PRNG so a property failure reproduces exactly.
function lcg(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0;
    return state / 0x100000000;
  };
}

// --- tests -------------------------------------------------------------------

describe("selectCutPoint", () => {
  it("returns null when there are not enough completed turns to keep", () => {
    const events = [...turn("a", 1), ...turn("b", 0), ...turn("c", 2)];
    expect(selectCutPoint(events, 3)).toBeNull();
  });

  it("returns null for an empty stream", () => {
    expect(selectCutPoint([], 3)).toBeNull();
  });

  it("keeps exactly keepRecentTurns turns after the cut", () => {
    const events = [
      ...turn("a", 1),
      ...turn("b", 1),
      ...turn("c", 1),
      ...turn("d", 1),
      ...turn("e", 1),
    ];
    const cut = selectCutPoint(events, 3);
    expect(cut).not.toBeNull();

    const cutIndex = events.findIndex((e) => e.id === cut?.upToEventId);
    const turnsAfter = events
      .slice(cutIndex + 1)
      .filter((e) => e.data.type === "turn_ended").length;
    expect(turnsAfter).toBe(3);
  });

  it("always cuts on a turn boundary", () => {
    const events = [...turn("a", 2), ...turn("b", 2), ...turn("c", 2)];
    const cut = selectCutPoint(events, 1);
    const cutEvent = events.find((e) => e.id === cut?.upToEventId);
    expect(cutEvent?.data.type).toBe("turn_ended");
  });

  it("reports how many events it compacts", () => {
    const a = turn("a", 1);
    const rest = [...turn("b", 1), ...turn("c", 1)];
    const cut = selectCutPoint([...a, ...rest], 2);
    expect(cut?.compactedEventCount).toBe(a.length);
  });

  it("never splits a tool round, over randomised streams", () => {
    const random = lcg(0x5eed);
    for (let trial = 0; trial < 200; trial += 1) {
      const turnCount = 1 + Math.floor(random() * 8);
      const events: Event[] = [];
      for (let t = 0; t < turnCount; t += 1) {
        events.push(...turn(`t${trial}-${t}`, Math.floor(random() * 4)));
      }
      const keep = Math.floor(random() * 4);
      const cut = selectCutPoint(events, keep);
      if (cut === null) continue;
      expect(splitsAToolRound(events, cut.upToEventId)).toBe(false);
    }
  });

  it("refuses a boundary that would strand an unfinished tool call", () => {
    // A turn that died mid-tool-call: a request with no result, and no
    // turn_ended. The only safe cut is the boundary before it.
    const clean = turn("a", 1);
    const alsoClean = turn("b", 1);
    const crashed = [
      messages("turn c"),
      event("tool_requested", {
        tool_call_id: "c-orphan",
        request: { function_name: "shell", arguments: {} },
      }),
    ];
    const events = [...clean, ...alsoClean, ...crashed];
    const cut = selectCutPoint(events, 1);
    expect(cut).not.toBeNull();
    expect(splitsAToolRound(events, cut!.upToEventId)).toBe(false);
  });
});

describe("shouldCompact", () => {
  const policy = DEFAULT_COMPACTION_POLICY;

  it("is disabled when the policy says so", () => {
    expect(
      shouldCompact({
        policy: { ...policy, enabled: false },
        promptTokens: 1_000_000,
        maxInputTokens: 1_000,
        materializedChars: 0,
      }),
    ).toBe(false);
  });

  it("fires once the prompt crosses the ratio of the input limit", () => {
    const args = {
      policy: { ...policy, thresholdRatio: 0.7 },
      maxInputTokens: 100_000,
      materializedChars: 0,
    };
    expect(shouldCompact({ ...args, promptTokens: 69_000 })).toBe(false);
    expect(shouldCompact({ ...args, promptTokens: 71_000 })).toBe(true);
  });

  it("falls back to a character budget when the limit is unknown", () => {
    const args = {
      policy: { ...policy, fallbackCharBudget: 1_000 },
      promptTokens: null,
      maxInputTokens: null,
    };
    expect(shouldCompact({ ...args, materializedChars: 999 })).toBe(false);
    expect(shouldCompact({ ...args, materializedChars: 1_001 })).toBe(true);
  });

  it("falls back when the provider reported no usage", () => {
    expect(
      shouldCompact({
        policy: { ...policy, fallbackCharBudget: 1_000 },
        promptTokens: null,
        maxInputTokens: 100_000,
        materializedChars: 1_001,
      }),
    ).toBe(true);
  });
});

describe("estimatedTokensFromChars", () => {
  // The pre-request trigger has no provider count to work from, so it estimates.
  // The estimate must lean high: compacting slightly early costs one summarizer
  // call, while under-estimating lets a prompt reach the hard limit — and that
  // failure is self-perpetuating, since the rejection happens before anything
  // can shrink the history that caused it.
  it("over-estimates rather than under-estimates", () => {
    // Real prompts run ~3.5-4 chars/token; this must not sit above that.
    expect(estimatedTokensFromChars(4_000)).toBeGreaterThan(1_000);
  });

  it("is monotonic and never rounds a non-empty prompt to zero", () => {
    expect(estimatedTokensFromChars(0)).toBe(0);
    expect(estimatedTokensFromChars(1)).toBe(1);
    expect(estimatedTokensFromChars(10_000)).toBeGreaterThan(
      estimatedTokensFromChars(9_999),
    );
  });
});

describe("capSummary", () => {
  it("leaves a summary within the cap untouched", () => {
    expect(capSummary("short", 100)).toBe("short");
  });

  it("keeps summary text rather than spending a tiny cap on the marker", () => {
    // The marker is ~22 chars. Below that, spending the budget on it leaves a
    // "summary" with no facts — non-empty, so the empty-summary guard lets it
    // through and the checkpoint replaces real history with nothing.
    for (let cap = 1; cap <= 25; cap += 1) {
      const capped = capSummary("the user asked about billing", cap);
      expect(capped.length).toBeLessThanOrEqual(cap);
      expect(capped.length).toBeGreaterThan(0);
      expect(capped.startsWith("\n...[")).toBe(false);
    }
    expect(capSummary("x".repeat(500), 100)).toContain("summary truncated");
  });

  it("hard-truncates an oversized summary", () => {
    const capped = capSummary("x".repeat(500), 100);
    expect(capped.length).toBeLessThanOrEqual(100);
  });

  it("keeps chained summaries bounded no matter how often it runs", () => {
    // The runaway-summary failure mode: each pass feeds the previous summary
    // back in. The cap, not the model, is what keeps this convergent.
    let summary = "";
    for (let i = 0; i < 50; i += 1) {
      summary = capSummary(
        `${summary}\nround ${i} ${"detail ".repeat(200)}`,
        8_000,
      );
      expect(summary.length).toBeLessThanOrEqual(8_000);
    }
  });
});

describe("checkpoint events", () => {
  it("round-trips a checkpoint through its event payload", () => {
    const checkpoint: CompactionCheckpoint = {
      upToEventId: "evt-000042",
      artifactId: "art-1",
      artifactPath: "compaction/conv-1/1.md",
      artifactVersion: 1,
      previousCheckpointId: null,
      compactedEventCount: 12,
      summaryChars: 400,
      promptTokensBefore: 150_000,
      model: "gpt-5.6-terra",
    };
    const parsed = checkpointFromEvent(checkpointEvent(toPayload(checkpoint)));
    expect(parsed).toEqual(checkpoint);
  });

  it("rejects a payload missing any required field", () => {
    // Rust declares these non-optional, so serde refuses a payload without
    // them. Defaulting here would let the two runtimes disagree about whether
    // the same event is valid — and a defaulted `compacted_event_count`
    // restarts the chain's cumulative total at zero, which is the number the
    // agent is shown to judge how much history it is missing.
    const complete = {
      up_to_event_id: "evt-1",
      artifact_id: "art-1",
      artifact_path: "compaction/1.md",
      artifact_version: 1,
      compacted_event_count: 4,
      summary_chars: 20,
      model: "m",
    };
    expect(checkpointFromEvent(checkpointEvent(complete))).not.toBeNull();
    for (const field of Object.keys(complete)) {
      const partial: Record<string, unknown> = { ...complete };
      delete partial[field];
      expect(checkpointFromEvent(checkpointEvent(partial))).toBeNull();
    }
  });

  it("rejects a malformed payload rather than half-reading it", () => {
    expect(checkpointFromEvent(checkpointEvent({}))).toBeNull();
    expect(
      checkpointFromEvent(checkpointEvent({ up_to_event_id: 42 })),
    ).toBeNull();
    expect(checkpointFromEvent({ type: "something_else" })).toBeNull();
    // An id-less checkpoint cannot resolve its summary; refuse it rather than
    // silently assemble a prompt with the compacted history missing.
    expect(
      checkpointFromEvent(
        checkpointEvent({
          up_to_event_id: "evt-1",
          artifact_path: "compaction/1.md",
          artifact_version: 1,
        }),
      ),
    ).toBeNull();
  });

  // The envelope, not the payload, is where the two runtimes previously
  // disagreed: TypeScript wrote a flattened `{type: "exo.compaction.v1", ...}`
  // that Rust's `EventData` enum rejects outright. These two cases pin the
  // shape from the outside so neither side can drift again.
  it("ignores a flattened checkpoint that is not a custom-event envelope", () => {
    expect(
      checkpointFromEvent({
        type: COMPACTION_CHECKPOINT_EVENT,
        ...toPayload({
          upToEventId: "evt-1",
          artifactId: "art-1",
          artifactPath: "compaction/1.md",
          artifactVersion: 1,
          previousCheckpointId: null,
          compactedEventCount: 1,
          summaryChars: 1,
          promptTokensBefore: null,
          model: "m",
        }),
      }),
    ).toBeNull();
  });

  it("decodes the shared cross-runtime fixture", async () => {
    // Same bytes the Rust suite deserializes; see tests/fixtures/README.md.
    const { readFile } = await import("node:fs/promises");
    const { fileURLToPath } = await import("node:url");
    const path = fileURLToPath(
      new URL(
        "../../tests/fixtures/compaction-checkpoint.json",
        import.meta.url,
      ),
    );
    const fixture = JSON.parse(await readFile(path, "utf8"));

    expect(checkpointFromEvent(fixture)).toEqual({
      upToEventId: "01920000-0000-7000-8000-000000000001",
      artifactId: "01920000-0000-7000-8000-0000000000a1",
      artifactPath:
        "compaction/01920000-0000-7000-8000-00000000000c/summary.md",
      artifactVersion: 3,
      previousCheckpointId: "01920000-0000-7000-8000-000000000002",
      compactedEventCount: 412,
      summaryChars: 6120,
      promptTokensBefore: 148000,
      model: "claude-sonnet-4-5",
    });
  });
});

/** A checkpoint event in the real custom-event envelope. */
function checkpointEvent(payload: Record<string, unknown>) {
  return {
    type: "custom",
    event_type: COMPACTION_CHECKPOINT_EVENT,
    payload,
  };
}

function toPayload(checkpoint: CompactionCheckpoint): Record<string, unknown> {
  return {
    up_to_event_id: checkpoint.upToEventId,
    artifact_id: checkpoint.artifactId,
    artifact_path: checkpoint.artifactPath,
    artifact_version: checkpoint.artifactVersion,
    previous_checkpoint_id: checkpoint.previousCheckpointId,
    compacted_event_count: checkpoint.compactedEventCount,
    summary_chars: checkpoint.summaryChars,
    prompt_tokens_before: checkpoint.promptTokensBefore,
    model: checkpoint.model,
  };
}

describe("resolveCompactionPolicy", () => {
  it("defaults to enabled with conservative settings", () => {
    expect(DEFAULT_COMPACTION_POLICY.enabled).toBe(true);
    expect(DEFAULT_COMPACTION_POLICY.thresholdRatio).toBe(0.7);
    expect(DEFAULT_COMPACTION_POLICY.keepRecentTurns).toBe(3);
    expect(DEFAULT_COMPACTION_POLICY.maxSummaryChars).toBe(8_000);
  });

  it("returns defaults for absent config", () => {
    expect(resolveCompactionPolicy(null)).toEqual(DEFAULT_COMPACTION_POLICY);
    expect(resolveCompactionPolicy(undefined)).toEqual(
      DEFAULT_COMPACTION_POLICY,
    );
  });

  it("overrides only the fields that are present", () => {
    const policy = resolveCompactionPolicy({ threshold_ratio: 0.5 });
    expect(policy.thresholdRatio).toBe(0.5);
    expect(policy.keepRecentTurns).toBe(
      DEFAULT_COMPACTION_POLICY.keepRecentTurns,
    );
  });

  it("clamps a nonsensical ratio into range", () => {
    expect(resolveCompactionPolicy({ threshold_ratio: 0 }).thresholdRatio).toBe(
      DEFAULT_COMPACTION_POLICY.thresholdRatio,
    );
    expect(resolveCompactionPolicy({ threshold_ratio: 5 }).thresholdRatio).toBe(
      1,
    );
  });
});

describe("CompactionGate", () => {
  const args = {
    policy: { ...DEFAULT_COMPACTION_POLICY, thresholdRatio: 0.7 },
    promptTokens: 90_000,
    maxInputTokens: 100_000,
    materializedChars: 0,
  };

  it("allows the first attempt when the threshold is crossed", () => {
    expect(new CompactionGate().shouldAttempt(args)).toBe(true);
  });

  it("allows nothing once an attempt has been made", () => {
    // Within one turn no new turn_ended event appears, so the cut point cannot
    // change: a second attempt would re-scan and re-summarize for the same
    // answer. Retrying every round of a long tool loop is real money.
    const gate = new CompactionGate();
    expect(gate.shouldAttempt(args)).toBe(true);
    gate.markAttempted();
    expect(gate.shouldAttempt(args)).toBe(false);
    expect(gate.shouldAttempt(args)).toBe(false);
  });

  it("still respects the threshold before the first attempt", () => {
    const gate = new CompactionGate();
    expect(gate.shouldAttempt({ ...args, promptTokens: 10_000 })).toBe(false);
  });
});
