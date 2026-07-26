// Conversation compaction policy for the TypeScript harness.
//
// A conversation's durable event log grows without bound, but a prompt cannot.
// Compaction bridges the two by writing a *checkpoint*: a custom event recording
// that everything up to some event id is now represented by a summary artifact.
// Prompt assembly then reads `instructions + summary + events after the
// checkpoint` instead of replaying the whole log. The log itself is never
// mutated, so history stays queryable and forking/time-travel keep working.
//
// This module is the pure half: cut-point selection, the trigger predicate, and
// the checkpoint payload codec. Everything here is I/O-free and directly
// testable. The I/O half lives in `index.ts` (read) and the turn loop (write).

import type {
  ArtifactVersion,
  Conversation,
  Event,
  EventData,
  Message,
  Turn,
} from "./index";
import {
  appendCustomEvent,
  HISTORY_EVENT_TYPES,
  materializeEventsToMessages,
  readActiveCheckpointEvent,
} from "./index";

export const COMPACTION_CHECKPOINT_EVENT = "exo.compaction.v1";
export const COMPACTION_FAILED_EVENT = "exo.compaction.failed.v1";

/**
 * Custom event carrying what a compaction's summarizer call cost.
 *
 * A *custom* event, not a `messages` one, and that is the whole point. Both
 * materializers treat every messages event as a turn boundary and flush pending
 * tool calls at it, so an accounting event that landed between a
 * `tool_requested` and its `tool_result` would make them fabricate a failure for
 * a call that succeeded and then append the real result as well.
 *
 * Writing it later does not fix that: turns on one conversation are not
 * serialized, so "no call is outstanding" is a claim about every in-flight turn,
 * not just the one doing the accounting. Custom events are ignored by prompt
 * assembly outright, so no ordering rule remains to get wrong.
 */
export const COMPACTION_USAGE_EVENT = "exo.compaction.usage.v1";

const TURN_ENDED = "turn_ended";

/** Marker appended when compaction runs, pointing at the summary artifact. */
export interface CompactionCheckpoint {
  /** Inclusive: retained history is everything strictly after this id. */
  upToEventId: string;
  /** Read directly by id; the alternative is a listArtifacts() scan per round. */
  artifactId: string;
  artifactPath: string;
  artifactVersion: number;
  /** Previous checkpoint in the chain, for auditing. */
  previousCheckpointId: string | null;
  compactedEventCount: number;
  summaryChars: number;
  promptTokensBefore: number | null;
  model: string;
}

export interface CompactionPolicy {
  enabled: boolean;
  /** Compact once the prompt exceeds this fraction of the model input limit. */
  thresholdRatio: number;
  /** Turns kept verbatim after the cut. */
  keepRecentTurns: number;
  /** Hard ceiling on summary size; the model is not trusted to respect it. */
  maxSummaryChars: number;
  /**
   * Model id used for summaries, within the agent's existing model binding. A
   * model id, not a binding name: the point is to use a cheaper model from the
   * same provider without extra configuration.
   */
  summaryModel: string | null;
  /**
   * Used when the price table has no input limit for the model.
   *
   * Deliberately sized for a *small* context window rather than a typical one.
   * This value is only reached when the model's real limit is unknown — an
   * unlisted model, or a price table that failed to download — so it has to be
   * safe for the smallest window it might stand in for. Guessing high on a 32k
   * model means the request is rejected, and because no response comes back the
   * accurate post-response trigger never runs: every later turn replays the same
   * oversized history and fails the same way. Guessing low just compacts earlier
   * than strictly necessary.
   */
  fallbackCharBudget: number;
}

export const DEFAULT_COMPACTION_POLICY: CompactionPolicy = {
  enabled: true,
  thresholdRatio: 0.7,
  keepRecentTurns: 3,
  maxSummaryChars: 8_000,
  summaryModel: null,
  fallbackCharBudget: 64_000,
};

export interface CutPoint {
  /** Inclusive id of the last event folded into the summary. */
  upToEventId: string;
  compactedEventCount: number;
}

/**
 * Choose where to cut, or null when the conversation is too short to bother.
 *
 * Cuts land only on `turn_ended` boundaries. That is what makes compaction safe:
 * at a turn boundary no tool call is outstanding, so no `tool_requested` can be
 * separated from its `tool_result`. Splitting a tool round would make the
 * materializer either fabricate a failure for a call that succeeded or silently
 * drop a result — both corrupt the model's view of what happened.
 *
 * `events` must be the ascending stream including `turn_ended` markers.
 */
export function selectCutPoint(
  events: Event[],
  keepRecentTurns: number,
): CutPoint | null {
  const keep = Math.max(0, Math.floor(keepRecentTurns));
  const boundaries: number[] = [];
  for (let i = 0; i < events.length; i += 1) {
    if (events[i].data.type === TURN_ENDED) {
      boundaries.push(i);
    }
  }
  // Need one boundary to cut at plus `keep` completed turns to leave behind.
  if (boundaries.length <= keep) {
    return null;
  }

  // Walk candidates newest-first from the deepest legal one. `turn_ended`
  // should already guarantee no pending tool call, but a log truncated by a
  // crash can violate that; fall back to an earlier boundary rather than emit
  // an unsafe cut.
  for (let c = boundaries.length - 1 - keep; c >= 0; c -= 1) {
    const index = boundaries[c];
    if (!hasPendingToolCall(events, index)) {
      return {
        upToEventId: events[index].id,
        compactedEventCount: index + 1,
      };
    }
  }
  return null;
}

/** True when some tool_requested at or before `index` has no tool_result yet. */
function hasPendingToolCall(events: Event[], index: number): boolean {
  const pending = new Set<string>();
  for (let i = 0; i <= index; i += 1) {
    const data = events[i].data;
    const callId = data.tool_call_id;
    if (typeof callId !== "string") continue;
    if (data.type === "tool_requested") {
      pending.add(callId);
    } else if (data.type === "tool_result") {
      pending.delete(callId);
    }
  }
  return pending.size > 0;
}

export interface ShouldCompactArgs {
  policy: CompactionPolicy;
  /** Provider-reported prompt tokens from the last response, if any. */
  promptTokens: number | null;
  /** The model's input limit, if the price table knows it. */
  maxInputTokens: number | null;
  /** Character count of the materialized prompt, for the fallback path. */
  materializedChars: number;
}

/**
 * Trigger predicate. Prefers the provider's own `prompt_tokens` against the
 * model's input limit — no client-side tokenizer needed, and it accounts for
 * whatever the provider actually counted. Falls back to a character budget when
 * either number is unavailable, since the price table is fetched over the
 * network and is explicitly best-effort.
 */
export function shouldCompact({
  policy,
  promptTokens,
  maxInputTokens,
  materializedChars,
}: ShouldCompactArgs): boolean {
  if (!policy.enabled) {
    return false;
  }
  if (promptTokens !== null && maxInputTokens !== null) {
    return promptTokens > policy.thresholdRatio * maxInputTokens;
  }
  return materializedChars > policy.fallbackCharBudget;
}

/**
 * Characters per token assumed when estimating a prompt's size without a
 * tokenizer.
 *
 * Deliberately low. English averages nearer four, but agent prompts are dense
 * with JSON and code, and the two errors are not symmetric: over-estimating
 * compacts a little earlier than strictly necessary, while under-estimating lets
 * a prompt reach the provider's hard limit — and that failure is
 * self-perpetuating, because the rejection happens before anything can shrink
 * the history that caused it.
 */
const ESTIMATED_CHARS_PER_TOKEN = 3;

/**
 * Rough token count for a prompt of `chars` characters.
 *
 * Only for the pre-request trigger, which has no provider-reported count to work
 * from. Once a response comes back, its usage is exact and preferred.
 */
export function estimatedTokensFromChars(chars: number): number {
  return Math.ceil(chars / ESTIMATED_CHARS_PER_TOKEN);
}

/**
 * Output-token ceiling for a summarizer request sized from `maxSummaryChars`.
 *
 * `capSummary` only truncates *after* a response has been generated,
 * transferred and billed, so on its own it bounds the stored summary but not
 * the latency, memory or cost of producing it. This bounds the request itself.
 *
 * Generous on purpose: the multiplier leaves room so a model that respects the
 * character instruction is never clipped mid-sentence, and `capSummary` remains
 * the exact ceiling.
 */
export function summarizerMaxOutputTokens(maxSummaryChars: number): number {
  const headroom = 2;
  const floorTokens = 256;
  return Math.max(
    floorTokens,
    estimatedTokensFromChars(maxSummaryChars) * headroom,
  );
}

/**
 * At-most-once-per-turn gate around `shouldCompact`.
 *
 * Compaction can only cut at a `turn_ended` boundary, and no new one appears
 * while a turn is in flight — so within a turn the cut point cannot change. A
 * second attempt would re-scan the log and re-run the summarizer to reach the
 * same answer. Without this, a turn that crosses the threshold and then fails
 * (or skips) compaction retries on every subsequent round, which on a long tool
 * loop is a real and silent cost.
 */
export class CompactionGate {
  private attempted = false;

  shouldAttempt(args: ShouldCompactArgs): boolean {
    return !this.attempted && shouldCompact(args);
  }

  markAttempted(): void {
    this.attempted = true;
  }
}

/**
 * Number of Unicode code points in a string.
 *
 * Not `.length`, which counts UTF-16 code units. Rust's `chars().count()` counts
 * code points, so measuring summaries with `.length` would make the two runtimes
 * disagree about the same text: an emoji-heavy summary truncates up to twice as
 * early in TypeScript, and `summary_chars` on the checkpoint would differ for a
 * byte-identical artifact.
 */
export function codePointCount(text: string): number {
  return Array.from(text).length;
}

/**
 * First `count` Unicode code points of a string.
 *
 * Not `.slice()`, which cuts by UTF-16 code unit and can land between the halves
 * of a surrogate pair — leaving a lone surrogate that renders as a replacement
 * character in the stored artifact.
 */
function sliceCodePoints(text: string, count: number): string {
  return Array.from(text).slice(0, count).join("");
}

/**
 * Enforce the summary ceiling. Chained compaction feeds each summary back into
 * the next one, so without a hard cap the summary itself grows without bound —
 * the classic way this design rots. Truncation is deliberately blunt: the model
 * gets one chance to respect the cap, and this is the backstop.
 *
 * Measured in code points, matching Rust's `cap_summary`.
 */
export function capSummary(summary: string, maxChars: number): string {
  const trimmed = summary.trim();
  if (codePointCount(trimmed) <= maxChars) {
    return trimmed;
  }
  const marker = "\n...[summary truncated]";
  // A cap too small to hold the marker *and* real content spends the whole
  // budget on the marker: the result is a prefix of "...[summary truncated]"
  // with no facts in it, and because that is non-empty the empty-summary guard
  // waves it through and checkpoints a cut whose summary says nothing. Keep the
  // summary instead; a short true summary beats a longer empty one.
  const markerChars = codePointCount(marker);
  if (maxChars <= markerChars) {
    return sliceCodePoints(trimmed, maxChars);
  }
  const head = maxChars - markerChars;
  return sliceCodePoints(
    `${sliceCodePoints(trimmed, head)}${marker}`,
    maxChars,
  );
}

export function checkpointToPayload(
  checkpoint: CompactionCheckpoint,
): Record<string, unknown> {
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

/**
 * Payload of a custom event of the given type, or null if `data` is some other
 * event.
 *
 * Custom events travel as `{ type: "custom", event_type, payload }` — that
 * envelope is the only extensible `EventData` variant the Rust harness accepts
 * (`crates/exoharness/src/types.rs`), and the variant tag is what makes an event
 * queryable by `event_type`. Reading `data.type` for the event name instead
 * would miss every event the Rust executor writes.
 */
function customEventPayload(
  data: EventData,
  eventType: string,
): Record<string, unknown> | null {
  if (data.type !== "custom" || data.event_type !== eventType) {
    return null;
  }
  const payload = data.payload;
  return typeof payload === "object" && payload !== null
    ? (payload as Record<string, unknown>)
    : null;
}

/**
 * True when `value` is a syntactically valid event id.
 *
 * Rust deserializes these as `Uuid7`, so a malformed id makes serde reject the
 * whole checkpoint and the reader safely replays the full log. Accepting any
 * string here would instead hand the bad cursor to `getEvents`, which rejects
 * the request and fails materialization outright — turning a recoverable
 * fallback into a hard error.
 */
function isEventId(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
      value,
    )
  );
}

/** True when a field is absent/null, or present with the expected type. */
function isAbsentOrType(
  value: unknown,
  expected: "string" | "number",
): boolean {
  return value === undefined || value === null || typeof value === expected;
}

/**
 * Decode a checkpoint event, or null if it is not one / is malformed. A partial
 * read would silently drop history, so every field is validated — required ones
 * for presence and type, optional ones for type when present.
 */
export function checkpointFromEvent(
  data: EventData,
): CompactionCheckpoint | null {
  const payload = customEventPayload(data, COMPACTION_CHECKPOINT_EVENT);
  if (payload === null) {
    return null;
  }
  // Every required field, not just the ones needed to locate the summary.
  // Rust's `CompactionCheckpoint` declares these non-optional, so serde refuses
  // a payload missing any of them; inventing a fallback here would let the two
  // runtimes disagree about whether the same event is even valid. It also
  // corrupts quietly: a defaulted `compacted_event_count` restarts the chain's
  // cumulative total at zero, and the agent is shown that number to judge how
  // much history it is missing.
  const upToEventId = payload.up_to_event_id;
  const artifactId = payload.artifact_id;
  const artifactPath = payload.artifact_path;
  const artifactVersion = payload.artifact_version;
  const compactedEventCount = payload.compacted_event_count;
  const summaryChars = payload.summary_chars;
  const model = payload.model;
  if (
    !isEventId(upToEventId) ||
    typeof artifactId !== "string" ||
    typeof artifactPath !== "string" ||
    typeof artifactVersion !== "number" ||
    typeof compactedEventCount !== "number" ||
    typeof summaryChars !== "number" ||
    typeof model !== "string"
  ) {
    return null;
  }
  // The optional fields are optional in *type*, not in validity. Rust models
  // them as `Option<T>`, and serde rejects the whole payload when a present
  // value has the wrong type — so coercing a bad value to null here would let
  // the two runtimes pick different histories for the same event: Rust falls
  // back to the full log, TypeScript honours a checkpoint it half-understood.
  if (
    payload.previous_checkpoint_id !== undefined &&
    payload.previous_checkpoint_id !== null &&
    !isEventId(payload.previous_checkpoint_id)
  ) {
    return null;
  }
  if (!isAbsentOrType(payload.prompt_tokens_before, "number")) {
    return null;
  }
  return {
    upToEventId,
    artifactId,
    artifactPath,
    artifactVersion,
    compactedEventCount,
    summaryChars,
    model,
    // Genuinely optional on both sides: absent on the first checkpoint of a
    // chain, and `null` when the provider reported no usage.
    previousCheckpointId:
      typeof payload.previous_checkpoint_id === "string"
        ? payload.previous_checkpoint_id
        : null,
    promptTokensBefore:
      typeof payload.prompt_tokens_before === "number"
        ? payload.prompt_tokens_before
        : null,
  };
}

/** Raw config shape as it arrives from the exoharness agent config. */
export interface RawCompactionConfig {
  enabled?: boolean | null;
  threshold_ratio?: number | null;
  keep_recent_turns?: number | null;
  max_summary_chars?: number | null;
  summary_model?: string | null;
  fallback_char_budget?: number | null;
}

export function resolveCompactionPolicy(
  raw: RawCompactionConfig | null | undefined,
): CompactionPolicy {
  if (!raw) {
    return { ...DEFAULT_COMPACTION_POLICY };
  }
  return {
    enabled: raw.enabled ?? DEFAULT_COMPACTION_POLICY.enabled,
    thresholdRatio: clampRatio(raw.threshold_ratio),
    keepRecentTurns: positiveIntOr(
      raw.keep_recent_turns,
      DEFAULT_COMPACTION_POLICY.keepRecentTurns,
    ),
    maxSummaryChars: positiveIntOr(
      raw.max_summary_chars,
      DEFAULT_COMPACTION_POLICY.maxSummaryChars,
    ),
    summaryModel: raw.summary_model ?? DEFAULT_COMPACTION_POLICY.summaryModel,
    fallbackCharBudget: positiveIntOr(
      raw.fallback_char_budget,
      DEFAULT_COMPACTION_POLICY.fallbackCharBudget,
    ),
  };
}

// A ratio of 0 or below would compact on every single round; above 1 would
// never fire before the provider rejects the request. Clamp rather than throw:
// a bad knob should degrade to the default, not brick the agent.
function clampRatio(value: number | null | undefined): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return DEFAULT_COMPACTION_POLICY.thresholdRatio;
  }
  return Math.min(1, value);
}

function positiveIntOr(
  value: number | null | undefined,
  fallback: number,
): number {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    return fallback;
  }
  return Math.floor(value);
}

// --- running a compaction ----------------------------------------------------

export interface SummarizeInput {
  /** Messages being folded into the summary — the compacted span only. */
  messages: Message[];
  /** Summary from the previous checkpoint, to be merged rather than replaced. */
  previousSummary: string | null;
  maxChars: number;
}

/**
 * Produces the summary text. Injected rather than called directly so the
 * orchestration below is testable without a model, and so callers can point it
 * at a cheaper model than the one running the conversation.
 */
export type SummarizeFn = (input: SummarizeInput) => Promise<string>;

export interface RunCompactionArgs {
  conversation: Conversation;
  turn: Turn;
  policy: CompactionPolicy;
  /** Model recorded on the checkpoint — the summarizer, not the agent model. */
  model: string;
  promptTokensBefore: number | null;
  summarize: SummarizeFn;
}

export type CompactionResult =
  | { status: "compacted"; checkpoint: CompactionCheckpoint }
  | { status: "skipped"; reason: string }
  | { status: "failed"; error: string };

/**
 * Fold a conversation's older history into a summary checkpoint.
 *
 * Nothing here is allowed to fail the caller's turn. Compaction is a
 * housekeeping step; if the summarizer is down or the artifact store rejects a
 * write, the right outcome is an oversized prompt (today's behaviour) rather
 * than a dead conversation. Failures are recorded as an event so the agent can
 * see why its context never shrank.
 */
export async function runCompaction(
  args: RunCompactionArgs,
): Promise<CompactionResult> {
  try {
    return await compact(args);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await recordFailure(args.turn, message);
    return { status: "failed", error: message };
  }
}

async function compact(args: RunCompactionArgs): Promise<CompactionResult> {
  const { conversation, turn, policy, model, promptTokensBefore } = args;

  const existing = await readActiveCheckpointEvent(conversation);
  const previousSummary = existing
    ? await conversation.readArtifactText({
        artifactId: existing.checkpoint.artifactId,
        version: existing.checkpoint.artifactVersion,
      })
    : null;

  // A checkpoint whose summary artifact cannot be read must not be chained off.
  // Scanning from its boundary would summarize only the tail, and the new
  // checkpoint would be perfectly readable — which disarms the read path's
  // safety net, where a missing artifact falls back to replaying the full log.
  // Everything before the broken checkpoint would then be gone from the prompt
  // for good. Rebuilding from the start costs one larger call and loses nothing.
  const previous =
    existing !== null && previousSummary === null ? null : existing;

  // Only look at events after the last checkpoint: everything before it is
  // already represented by `previousSummary`.
  const scan = await conversation.getEvents({
    direction: "asc",
    cursor: previous?.checkpoint.upToEventId ?? null,
    types: [...HISTORY_EVENT_TYPES, "turn_ended"],
  });
  const cut = selectCutPoint(scan.events, policy.keepRecentTurns);
  if (cut === null) {
    return { status: "skipped", reason: "not enough completed turns to cut" };
  }

  const cutIndex = scan.events.findIndex((e) => e.id === cut.upToEventId);
  const compactedEvents = scan.events.slice(0, cutIndex + 1);
  const compactedMessages = materializeEventsToMessages(compactedEvents);

  // A prompt can cross the threshold because of the turns being *kept* — one
  // huge tool result, say. Replacing a smaller prefix with a summary that could
  // be larger grows the prompt instead of shrinking it, and spends a model call
  // to do so. Nothing to reclaim means nothing to do.
  const spanChars = messagesChars(compactedMessages);
  const previousChars =
    previousSummary === null ? 0 : codePointCount(previousSummary);
  if (spanChars + previousChars <= policy.maxSummaryChars) {
    return {
      status: "skipped",
      reason: "compactable history is already smaller than the summary cap",
    };
  }

  const summarized = await args.summarize({
    messages: compactedMessages,
    previousSummary,
    maxChars: policy.maxSummaryChars,
  });

  const summary = capSummary(summarized, policy.maxSummaryChars);
  if (summary.length === 0) {
    // Checkpointing an empty summary would drop real history and put nothing
    // in its place — strictly worse than leaving the prompt large.
    const error = "summarizer returned an empty summary";
    await recordFailure(turn, error);
    return { status: "failed", error };
  }

  const written: ArtifactVersion = await conversation.writeArtifactText({
    path: summaryArtifactPath(conversation),
    text: summary,
  });

  const checkpoint: CompactionCheckpoint = {
    upToEventId: cut.upToEventId,
    artifactId: written.artifactId,
    artifactPath: written.path,
    artifactVersion: written.version,
    // The previous *checkpoint event's* own id, not its cut boundary. The
    // boundary names an ordinary `turn_ended` event, so storing it here makes
    // the chain untraversable from the second compaction onward.
    previousCheckpointId: previous?.eventId ?? null,
    // Cumulative across the chain. This is the number the agent is shown to
    // judge how much history it is missing, so counting only this pass would
    // understate it on every compaction after the first.
    compactedEventCount:
      (previous?.checkpoint.compactedEventCount ?? 0) + cut.compactedEventCount,
    summaryChars: codePointCount(summary),
    promptTokensBefore,
    model,
  };
  await appendCustomEvent(
    turn,
    COMPACTION_CHECKPOINT_EVENT,
    checkpointToPayload(checkpoint),
  );
  return { status: "compacted", checkpoint };
}

function messagesChars(messages: Message[]): number {
  let total = 0;
  for (const message of messages) {
    total +=
      typeof message.content === "string"
        ? message.content.length
        : JSON.stringify(message.content).length;
  }
  return total;
}

function summaryArtifactPath(conversation: Conversation): string {
  return `compaction/${conversation.record.id}/summary.md`;
}

async function recordFailure(turn: Turn, error: string): Promise<void> {
  try {
    await appendCustomEvent(turn, COMPACTION_FAILED_EVENT, { error });
  } catch {
    // Recording the failure is best-effort; it must not mask the original
    // problem or take the turn down with it.
  }
}
