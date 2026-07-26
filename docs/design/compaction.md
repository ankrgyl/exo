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

The provider already tells us how large the prompt was: `prompt_tokens` on the
response usage, which the harness records on every `messages` event. Compare it
against the model's `max_input_tokens`, which is already present in the LiteLLM
data the cost table downloads:

```
prompt_tokens > thresholdRatio * max_input_tokens   → compact
```

No client-side tokenizer, and the number reflects what the provider actually
counted. When either value is unavailable — the price table is fetched over the
network and is explicitly best-effort — a character budget stands in.

Compaction runs _between rounds_, not at turn start, so a single runaway turn
can bring its own prompt back under the limit.

## Summaries

The summarizer is a model call with no tools. On the second and later
compactions it receives the previous summary and is asked to **merge** rather
than append, so a long conversation converges on a fixed-size summary instead of
accumulating one paragraph per compaction.

`maxSummaryChars` is enforced in code, not by asking the model nicely.
Unbounded recursive summarization is the standard way this design rots.

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

## Caching

`BasicExecutor` keeps an incremental history cache, and the TypeScript turn loop
now has the equivalent (`PromptHistoryCache`) — the loop materializes on every
round, so re-reading the whole log each time made a turn cost
O(rounds × events).

**Compaction must invalidate that cache.** It replaces precisely the prefix the
cache holds; a stale cache would keep serving pre-compaction history from memory,
the prompt would never shrink, and nothing would error. Both implementations have
a test for this, and stubbing out the invalidation makes it fail.

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

## Known limits

- Compaction bounds the _prompt_, not the _read_.
  `crates/exoharness/src/storage.rs` still loads every event file from disk
  before filtering, so a cursor query remains O(N) at the storage layer.
  Indexing the event log is the complementary fix.
- Summaries are flat, not hierarchical. If a flat summary proves lossy in
  practice, tiered summaries are the next step.
- Summary _quality_ is not covered by the test suite — the deterministic tests
  inject a fake summarizer. A fact-retention eval (plant facts before the cut,
  check recall after) belongs in Braintrust, which is already wired through
  every runtime.
