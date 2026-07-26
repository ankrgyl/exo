# Conversation compaction

A conversation's durable event log grows without bound. A prompt cannot. Without
something bridging the two, a long-running agent eventually exceeds the model's
input limit — and then _every_ subsequent turn fails, permanently, because each
one replays the same oversized history. That is not slow degradation; it is a
conversation that can never be used again.

Compaction bridges the gap without touching the log.

## Mechanism

A custom event, `exo.compaction.v1`, records that everything up to some event id
is now represented by a summary artifact. Prompt assembly becomes:

```
instructions
+ summary            (from the newest checkpoint, if any)
+ events after the checkpoint
```

The raw event log is never mutated. History stays queryable, forking and time
travel keep working, and anything a summary loses is still recoverable. This is
the mechanism [`docs/spec.md`](../spec.md) prescribes: _"to implement compaction,
an executor can write a custom event that points at a derived context view or
summary."_

Finding the active checkpoint is one bounded query
(`getEvents({direction: "desc", limit: 1, types: ["exo.compaction.v1"]})`), and
the checkpoint carries the artifact id so reading the summary is a direct fetch
rather than a `listArtifacts` scan.

### The wire format is a custom-event envelope

A checkpoint travels as `{type: "custom", event_type: "exo.compaction.v1",
payload: {...}}`. That envelope is not cosmetic: `Custom` is the only extensible
variant of Rust's `EventData` enum, which is `#[serde(tag = "type")]`, so a
flattened `{type: "exo.compaction.v1", ...}` is rejected as an unknown variant.
Querying by `event_type` works because a custom event's kind _is_ its
`event_type` (`EventData::kind()`).

TypeScript cannot catch a mistake here at compile time — its `EventData` is
`{type: string} & Record<string, unknown>` — and each runtime's own tests will
happily agree with whatever shape that runtime writes. So the format is pinned
by a golden fixture that _both_ test suites parse:
[`tests/fixtures/compaction-checkpoint.json`](../../tests/fixtures/compaction-checkpoint.json).

Two payload fields store event ids, and both are cursors rather than data:
`up_to_event_id` (the cut boundary) and `previous_checkpoint_id` (the previous
checkpoint _event's_ id, which makes the chain traversable — storing the previous
cut boundary there would name an ordinary `turn_ended` event). Because forking
renumbers every event it copies, `EventData::remap_event_ids` rewrites both while
the fork is being written; otherwise a forked checkpoint points into the source
conversation, and since the regenerated ids all sort _after_ the stale cursor,
the fork replays its entire history _and_ prepends the summary.

## Where cuts are allowed

**Cuts land only on `turn_ended` boundaries.** This is the load-bearing
constraint, and it is not an optimisation.

Look at what the materializer does with a split tool round
(`typescript/harness/index.ts`, `crates/executor/src/basic.rs`):

- A `tool_requested` whose `tool_result` falls outside the window →
  a `{ok: false, error: "tool execution did not complete..."}` result is
  fabricated. **The model is told a tool failed that actually succeeded.**
- A `tool_result` whose `tool_requested` falls outside the window → silently
  dropped.

The first is the dangerous one: it is invisible and it corrupts the model's
understanding of what happened. At a turn boundary no tool call is outstanding,
so neither can occur. `selectCutPoint` additionally verifies no call is pending
and walks back to an earlier boundary if one is — a log truncated by a crash can
violate the invariant that `turn_ended` otherwise guarantees.

A property test over randomised event streams covers this in both languages.
Mutating the cut point to land mid-round makes it fail.

## When compaction triggers

The provider already tells us how large the prompt was, so no client-side
tokenizer is needed. Compare occupancy against the model's `max_input_tokens`,
which is already present in the LiteLLM data the cost table downloads:

```
inputOccupancy > thresholdRatio * max_input_tokens   → compact
```

**Occupancy is not `prompt_tokens`.** For Anthropic-family providers that number
counts only the _fresh_ slice; cache reads and cache writes are reported
separately and billed additively — the same asymmetry `computeCostUsd` already
handles via `isAdditive()`. A 195k-token prompt that is 185k cache hits reports
`prompt_tokens ≈ 5000`, so a threshold check against the fresh slice alone would
sit at 2.5% of a 200k window and never fire — on precisely the workload
compaction exists for. `inputOccupancy` reuses `isAdditive()` to add the cache
fields back for those providers, and trusts `prompt_tokens` for the inclusive
ones (where adding them would double-count).

When either value is unavailable — the price table is fetched over the network
and is explicitly best-effort — a character budget stands in.

### Two triggers, and why both are needed

**After each round**, from the provider's own counts. This is the accurate one,
and it runs between rounds rather than at turn start so a single runaway turn can
bring its own prompt back under the limit.

**Before each request**, from a character estimate of the assembled prompt. This
one exists because the post-response trigger cannot fire on the case that matters
most: a prompt already past the hard limit is _rejected_, and that error leaves
the turn before any compaction runs. The next turn replays the same oversized
history and fails identically — an absorbing state that no retry escapes. The
character estimate is deliberately pessimistic (3 chars/token) because the two
errors are asymmetric: compacting slightly early costs one summarizer call, while
failing to fire costs the conversation.

Both share a once-per-turn latch. No new `turn_ended` appears mid-turn, so the
cut point cannot change within a turn; a second attempt would re-scan the log and
re-run the summarizer for the same answer.

### RLM does not compact, on purpose

The RLM executor is the exception, and it is worth being precise about why —
"both executors compact" is the intuitive answer and it is wrong.

RLM's transcript is **out-of-band**. `build_rlm_root_prompt` embeds only a
character count and a ~400-character preview; the full text goes into the JS
REPL's `context` variable, which the model reaches through `repl_execute` rather
than by reading it in the prompt. So the transcript never occupies the context
window, and its size does not move the root prompt's size.

Compacting it would therefore reclaim nothing and cost the thing this executor
exists for: precise access to a large external context without spending the
window on it. `AgentConfig::compaction` documents itself as basic-executor-only,
and `rlm_does_not_compact_its_out_of_band_transcript` pins the behaviour.

For the same reason `materialize_conversation_messages` — used by RLM and by
`HarnessConversation::messages()` — deliberately returns the **full** log and
ignores checkpoints. It is not a prompt builder. Making it checkpoint-aware would
also quietly break the "history stays queryable" guarantee that justifies never
mutating the log.

## Summaries

The summarizer is a model call with no tools. On the second and later
compactions it receives the previous summary and is asked to **merge** rather
than append, so a long conversation converges on a fixed-size summary instead of
accumulating one paragraph per compaction.

`maxSummaryChars` is enforced in code, not by asking the model nicely.
Unbounded recursive summarization is the standard way this design rots.

### The summary is not an instruction

A summary is presented as a **user** message, wrapped in `<conversation_summary>`
— not as a system or developer message.

The compacted span is user turns, assistant turns and tool output: content an
outside party can write, including text shaped like an instruction. Rendering the
summary at system priority would give that content more authority _after_
compaction than it had before, turning routine housekeeping into a privilege
escalation — and one that only manifests on long conversations, where it is
hardest to notice. `user` is the ceiling of what went into the summary, since
instructions are rebuilt every round and never sourced from events. The envelope
tells the model it is reading a record rather than a request.

## What survives compaction

- **Instructions** — rebuilt every round, never sourced from events.
- **Memory, todos, skills** — re-injected each turn from artifacts, so they live
  outside the event stream entirely.
- **The last `keepRecentTurns` turns**, verbatim.

## Failure policy

Compaction never fails a turn. A summarizer outage, a rejected artifact write,
or an empty summary all leave the prompt oversized — which is the behaviour
before this feature existed — rather than killing the conversation. Failures are
recorded as `exo.compaction.failed.v1` so the agent can see why its context
stopped shrinking.

An empty summary is refused outright: checkpointing one would drop real history
and put nothing in its place, which is strictly worse than a large prompt. A
checkpoint whose artifact has vanished falls back to full history for the same
reason.

That fallback is also why an unreadable summary **stops the chain**. Compacting
from a broken checkpoint's boundary would summarize only the tail and then write
a perfectly readable checkpoint over it — disarming the fallback and dropping
everything before the break from the prompt for good. Instead, compaction
rebuilds from the start of the log: one larger summarizer call, nothing lost.

The summarizer is a real, billable call, so its usage is recorded — on a
`messages` event, which is where this repo's cost aggregation looks. The message
list is empty on purpose: history materialization folds these events into the
prompt, so carrying the summarizer's reply there would inject it back into the
context compaction just shrank.

## Caching

`BasicExecutor` keeps an incremental history cache, and the TypeScript turn loop
now has the equivalent (`PromptHistoryCache`) — the loop materializes on every
round, so re-reading the whole log each time made a turn cost
O(rounds × events).

**Compaction must invalidate that cache.** It replaces precisely the prefix the
cache holds; a stale cache would keep serving pre-compaction history from memory,
the prompt would never shrink, and nothing would error. Both implementations have
a test for this, and stubbing out the invalidation makes it fail.

Invalidation alone is not sufficient, because turns on one conversation are not
serialized. A turn that read a pre-checkpoint entry and then blocked in
`getEvents` would, on resuming, write its stale snapshot back _over_ the
invalidation — and that entry carries its own cursor and `summary: None`, so
every later prompt keeps replaying the compacted prefix indefinitely. The Rust
cache therefore carries a generation counter, bumped on every invalidation and
re-checked before publishing: a read that started in an older generation drops
its result instead of republishing it.

The TypeScript cache holds raw _events_ and re-folds them each round rather than
caching derived messages. The fold is cheap and the fetch is what hurts, and it
keeps output identical to an uncached materialization by construction —
including tool rounds that span a batch boundary.

## Configuration

| Field                | Default       | Meaning                                                   |
| -------------------- | ------------- | --------------------------------------------------------- |
| `enabled`            | `true`        | Off means unbounded prompts.                              |
| `thresholdRatio`     | `0.7`         | Fraction of the input limit that triggers compaction.     |
| `keepRecentTurns`    | `3`           | Turns kept verbatim after the cut.                        |
| `maxSummaryChars`    | `8000`        | Hard ceiling on summary size.                             |
| `summaryModel`       | agent's model | Model id (within the agent's binding) used for summaries. |
| `fallbackCharBudget` | `400000`      | Used when the model's input limit is unknown.             |

From the CLI:

```bash
exo agent create my-agent --model gpt-5.6-terra \
  --compaction-threshold-ratio 0.6 \
  --compaction-keep-recent-turns 5 \
  --compaction-summary-model cheap-model

exo agent update my-agent --no-compaction
```

Compaction is purely additive: a conversation with no checkpoint behaves exactly
as it did before, so no migration is needed.

## Agent-facing tools

Compaction is the only harness policy that silently removes things from the
agent's own view of its history, so it is made inspectable:

- `describe_compaction` — effective policy and active checkpoint.
- `read_compaction_summary` — the summary text in full.
- `list_conversation_events` — the raw history behind the summary; checkpoint
  and failure events appear in its default lifecycle view.

A prompt block appears only once a checkpoint exists, stating how many events
were folded away so the agent can judge what it is missing rather than guess.

Compaction also does nothing when the compactable span is already smaller than
`maxSummaryChars`. A prompt can cross the threshold because of the turns being
_kept_ — one enormous tool result, say — and replacing a smaller prefix with a
summary that could be larger would grow the prompt, not shrink it.

Each turn attempts compaction at most once. Cuts land only on `turn_ended`
boundaries and none appears mid-turn, so the answer provably cannot change
within a turn; retrying every round would re-scan the log and re-run the
summarizer for the same result.

## Known limits

- Compaction bounds the _prompt_, not the _read_.
  `crates/exoharness/src/storage.rs` still loads every event file from disk
  before filtering, so a cursor query remains O(N) at the storage layer.
  Indexing the event log is the complementary fix.
- A conversation whose _retained_ turns alone exceed the input limit cannot be
  rescued by compaction: there is nothing safe left to cut. Lowering
  `keepRecentTurns` helps; cutting mid-turn would corrupt tool rounds.
- Summaries are flat, not hierarchical. If a flat summary proves lossy in
  practice, tiered summaries are the next step.
- Summary _quality_ is not covered by the test suite — the deterministic tests
  inject a fake summarizer. A fact-retention eval (plant facts before the cut,
  check recall after) belongs in Braintrust, which is already wired through
  every runtime.
