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

Agreeing on the bytes is not sufficient; the two decoders must also agree on what
is _valid_. Rust's serde rejects a payload whose field has the wrong type,
including an optional one, and a rejected checkpoint falls back to the full log.
TypeScript therefore validates every field rather than coercing — otherwise the
same event yields a checkpoint on one runtime and a full replay on the other.
Sizes are counted in **code points** on both sides (`chars().count()`,
`Array.from(...).length`), not UTF-16 units, or an emoji-heavy summary truncates
twice as early in TypeScript and `summary_chars` disagrees for identical text.

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

**A `turn_ended` marker proves only that its own turn ended.** Turns on one
conversation are not serialized, so another turn can have appended its user
message and be waiting on a model response when that marker lands. Cutting there
would fold the other turn's own request into the summary, and its next round
would materialize a prompt where its verbatim input has been replaced by a
paraphrase — while its later events keep arriving after the cut. So a boundary is
usable only when every turn open before it has also closed. The pending-tool-call
check cannot see this case: the other turn has not requested a tool yet, and may
never. Both markers therefore have to be in the scan query — dropping
`turn_started` does not fail loudly, it just makes the check blind.

**But "unfinished" has to age out.** A process that dies mid-turn leaves markers
nothing will ever balance — a `turn_started` with no `turn_ended`, or a
`tool_requested` with no `tool_result`. Honouring either forever means compaction
is permanently dead on that conversation: it grows until the model refuses it,
with no way back. A cut landing _before_ the orphan is what makes that permanent,
since every later scan starts at the checkpoint and still contains it. That is
strictly worse than the failures these checks prevent, which are all recoverable.
Unfinished work therefore stops blocking once `ABANDONED_WORK_GRACE` (8) turns
have _completed_ since it began — one constant for both checks, because it is the
same question with the same answer.

The grace is easier to justify for a stranded tool call than for an open turn.
Cutting across a _live_ call makes the materializer fabricate a
`{ok: false, "tool execution did not complete"}` for a call that succeeded; for
an _abandoned_ one that fabricated result is simply true. Note also where that
check does its work: while the requesting turn is still open the pending-turn
check refuses the boundary anyway, so the tool-call grace only decides the case
where a turn _ended_ leaving a call unresolved — a crashed or truncated log,
essentially by definition.

Turns are matched by `turn_id`, not by counting starts against ends. A plain
counter cannot tell _which_ start is unmatched: after a crash, later turns'
`turn_ended` markers balance the abandoned one and the imbalance appears to
belong to the newest turn — which is exactly the turn that never ages out. Where
the harness leaves a marker unattributed, the fallback matches newest-first, so
an abandoned start sinks to the bottom of the stack and ages rather than being
handed every subsequent turn's end.

A property test over randomised event streams covers the tool-round invariant in
both languages. Mutating the cut point to land mid-round makes it fail; so does
dropping the open-turn check from an overlapping-turn fixture. Both graces are
pinned from both sides: removing one lets an abandoned marker block forever, and
shrinking it lets a boundary cut across work that is still running.

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

Occupancy counts cache **writes** as well as cache reads. Anthropic reports
`cache_creation_input_tokens` separately from `input_tokens`, and those tokens
occupy the window like any other — ignoring them makes the turn that fills the
cache look small at exactly the moment it is largest.

When either value is unavailable — the price table is fetched over the network
and is explicitly best-effort — `fallbackCharBudget` stands in. That budget is
deliberately sized for a _small_ window (~32k tokens), not a typical one: it is
only reached when the real limit is unknown, so it has to be safe for the
smallest model it might be standing in for. Guessing high gets the request
rejected, and with no response the accurate trigger never runs; guessing low just
compacts earlier than necessary.

**The fallback compares estimated tokens, not raw bytes.** The knob is a byte
figure, but bytes are not what fills a context window: the same 64KB is ~21k
tokens of ASCII and ~32k of Hangul or emoji. Comparing bytes therefore let
exactly the scripts that tokenize densest sail past a small window while the
trigger reported slack — the same unit confusion the preflight measurement had,
surviving in the one branch that runs when nothing can check the model's real
limit. The budget is converted at the ASCII rate, so an ASCII prompt still fires
at the documented number of bytes and a denser script fires earlier.

### Two triggers, and why both are needed

**After each round**, from the provider's own counts. This is the accurate one,
and it runs between rounds rather than at turn start so a single runaway turn can
bring its own prompt back under the limit.

**Before each request**, from a size estimate of the assembled prompt. This one
exists because the post-response trigger cannot fire on the case that matters
most: a prompt already past the hard limit is _rejected_, and that error leaves
the turn before any compaction runs. The next turn replays the same oversized
history and fails identically — an absorbing state that no retry escapes.

The estimate covers **messages and tool schemas**. Tools ride in the same request
and consume the same window, and a harness can register a lot of them — sizing by
messages alone lets a conversation sit under the threshold on message text while
the request that actually goes out is over the limit, which is the very failure
this trigger exists to catch.

#### Measuring a prompt without a tokenizer

`PromptSize` counts **UTF-8 bytes, split at ASCII**, and that shape is
load-bearing twice over.

Bytes, not characters, because a token is far closer to a fixed number of bytes
than to a fixed number of characters. `String.length` in JavaScript is not even
characters — it is UTF-16 code units, so a CJK ideograph reads as 1 where the
wire carries 3. Measuring that way reported a third of true size for CJK-heavy
prompts, and reporting a third of true size is exactly how a request sails past
the hard limit with the trigger showing slack.

Split at ASCII, because one byte-per-token ratio does not fit every script. ASCII
prose runs about four bytes to the token and is charged at three — deliberately
pessimistic, since the two errors are asymmetric: compacting slightly early costs
one summarizer call, while failing to fire costs the conversation. Outside ASCII
a character is two to four bytes and rarely cheaper than a token (a CJK ideograph
is three bytes and usually one token, a Hangul syllable three bytes and often
two, an emoji four bytes and sometimes several), so those bytes are charged at
two.

This is an estimate with a known error bound, not a tokenizer. It over-charges
CJK by roughly half and can still under-charge the densest Hangul. The accurate
provider count remains the real mechanism; this only has to be right enough to
keep a prompt away from the wall.

Both share a latch, because a second attempt within a turn re-scans the log and
can re-run the summarizer — real money on a long tool loop. But the latch records
_why_ re-attempting would be pointless rather than asserting it. The obvious
version — "no new `turn_ended` appears mid-turn, so the cut point cannot change"
— is the unserialized-turns premise being violated again: other turns finish
while this one loops. One early "not enough completed turns to cut" would then
suppress every later check while the prompt kept growing. So the latch stores the
newest turn boundary at the last attempt; a new one means the answer may have
changed, the same one means it cannot have.

The boundary is most of that and not all of it. A skip is deterministic _given
the pressure it was asked under_, and a rescue deliberately ignores the cost
heuristic a housekeeping skip relies on — so a turn that skipped at a boundary
under the threshold, then had a large tool result push it past the hard limit,
is asking a different question at the same boundary. Crossing into rescue
reopens the latch; the reverse does not, since a rescue already answers the
housekeeping question.

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

For the same reason `materialize_conversation_messages` (Rust) and
`materializeConversationMessages` (TypeScript) — used by RLM and by the "what is
in this conversation" accessors — deliberately return the **full** log and ignore
checkpoints. They are not prompt builders. Making them checkpoint-aware would
also quietly break the "history stays queryable" guarantee that justifies never
mutating the log.

The two purposes have to be separate _functions_, not a flag: TypeScript's
`materializeConversationMessages` originally served both the RLM context and two
genuine prompt paths, so it applied the checkpoint and the RLM harness silently
received a summary instead of its exact history. `materializePromptHistory` is
now the checkpoint-aware one, and the split matches Rust's.

## Summaries

The summarizer is a model call with no tools. On the second and later
compactions it receives the previous summary and is asked to **merge** rather
than append, so a long conversation converges on a fixed-size summary instead of
accumulating one paragraph per compaction.

`maxSummaryChars` is enforced in code, not by asking the model nicely.
Unbounded recursive summarization is the standard way this design rots. It also
bounds the request itself, via `summarizerMaxOutputTokens` — `capSummary` alone
truncates only after a response has been generated, transferred and billed.

### The summarizer's own context window

`summaryModel` exists to run summaries on something cheaper than the agent's
model, and cheaper models routinely have smaller input windows. Compaction fires
at a share of the **agent** model's limit, so the span it hands the summarizer
can be well inside budget for the agent and well over the summary model's — and
the request is rejected. Because compaction failures are non-fatal by design,
the only symptom would be a conversation that stops compacting exactly when it
has grown large enough to need it.

`resolveSummarizerModel` therefore falls back to the agent's own model when the
prompt does not fit the configured one — reserving the summarizer request's own
fixed overhead as it does, since that request is not a subset of the agent's:
the agent instructions come out and the summarizer's instruction and merge
wrapper go in. The agent's model fits by construction:
it was carrying that prompt a moment ago. The yardstick is the whole prompt
rather than the span that will actually be summarized — an over-estimate, since
the span excludes the kept turns and the tool schemas, but the span is not known
until a cut point has been chosen, which happens after the model id is fixed and
recorded on the checkpoint. Erring towards the agent's model costs money on one
summary; erring the other way costs the compaction.

This does not address a span too large for **any** available model — see
_Known limits_.

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

The same rule applies one step earlier, to the **summarizer's own** prompt. When
a previous summary is merged in, it arrives as a delimited user message ahead of
the material — never spliced into the summarizer's system instruction. That call
is the one that decides what survives into every later prompt, and whatever it
produces gets re-merged into every subsequent summary, so it is the worst
possible place to hand conversation-derived text the harness's authority.

Because the wrapper is part of what lands in the prompt, it is also part of the
"is this worth doing" arithmetic: the no-growth guard compares the span against
`maxSummaryChars` **plus** the envelope, not against the cap alone. Comparing
bare summary text to enveloped span text makes the guard too permissive by
exactly the wrapper's size — precisely the band where compaction is least likely
to pay for itself.

The guard has a second unit trap, easy to miss because both numbers look like
sizes. The span is measured in serialized **bytes**; the cap counts
**characters**. An 8000-character emoji summary is 32KB, so a 9KB span compared
naively against an 8000 cap looks like a clear win and would quadruple the
prompt. Pricing the cap in bytes needs a bytes-per-character rate, and that rate
is taken from the span itself rather than assumed worst-case: a summary is
written in the same script as the material it summarizes, so an ASCII
conversation prices at ~1 byte per character and a CJK one at 3 — where a fixed
worst-case 4 would stop ASCII conversations compacting until their spans reached
32KB.

Both sides of that comparison have to be in the same unit, and the unit is
**serialized bytes**. The span is measured after JSON encoding, so the summary
has to be too: measuring its raw text undercounts it by every quote, backslash
and newline the encoder has to escape, and a summary of quoted code is mostly
those. `summary_message_size` measures the message the prompt will actually
carry, envelope included, and the previous summary is measured the same way.

**That rate is a heuristic, so the question is asked twice.** "A summary is
written in the script it summarizes" is usually true and cannot be relied on: a
summary that reaches for another script is 4 bytes per character where the span
was 1. So the estimate stays a cheap pre-filter — its job is to avoid paying for
a summarizer call that obviously cannot pay for itself — and a second check runs
once the summary exists and can be _measured_ rather than predicted. A summary
that came back larger than the history it would replace is discarded, already
paid for, rather than published: a checkpoint would persist the enlarged prompt
until the next cut.

That measurement compares **bytes and estimated tokens**, not bytes alone.
Shrinking one does not imply shrinking the other, and the context window is
denominated in tokens: a 24KB ASCII span is ~8k tokens, while a 5000-emoji
summary is only 20KB but ~10k tokens — smaller on the wire and larger in the
window. Bytes still govern what is stored and transferred, so the replacement
has to win on both rather than trade one against the other.

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
reason — and so does one whose artifact the store _refuses_, on the read path,
on the write path (where an unreadable previous summary rebuilds from the start
of the log rather than failing the compaction), and in `compactionInstruction`,
which runs while instructions are assembled and so gets no later chance to fall
back.

The rule covers the query that finds the checkpoint, not only the artifact behind
it. A failed checkpoint lookup is treated as "no checkpoint" and logged: the raw
messages are readable, so failing the turn over optional compaction metadata is
the same trade, and that query runs before anyone knows whether the conversation
even has a checkpoint or has compaction switched off. No extra retry logic is
needed for the cache — an entry records the checkpoint id it was built against,
and the `None` recorded during an outage stops matching the moment the query
recovers.

**Falling back gracefully in two places is not the same as agreeing.** The notice
and the prompt read the same summary, moments apart, and two independent reads
can land on different sides of a transient failure: the notice's succeeds, the
prompt's does not, and the agent is handed the full raw log underneath a
developer message insisting the older part was replaced by a summary above it —
the exact failure the notice exists to prevent, reintroduced by reading twice.
Successful reads are therefore memoized by `artifactId@version`. That key is
immutable content, so a hit can never be stale and no invalidation is needed;
only successes are kept, since memoizing "could not read" is the mistake
`SummaryRead`'s third state exists to stop. The remaining disagreement is the
harmless direction — a prompt that has a summary the notice did not announce.

A missing artifact and an erroring read are different failures but the same
situation _for the prompt_: the raw log is intact either way, so propagating the
error would take a working conversation down over something recoverable, and take
it down repeatedly, since every later turn consults the same checkpoint. Losing
the summary costs prompt space; losing the turn costs the agent.

They are **not** the same situation for a cache. A missing artifact is a fact
about the conversation and will not change; an errored read is a fact about right
now. Remembering the second as though it were the first means never retrying, so
one blip turns into a full-history replay that outlives it — and the Rust history
cache outlives the turn. Hence `SummaryRead` has three states, and a cache primes
only on a conclusive one.

That fallback is also why an unreadable summary **stops the chain**. Compacting
from a broken checkpoint's boundary would summarize only the tail and then write
a perfectly readable checkpoint over it — disarming the fallback and dropping
everything before the break from the prompt for good. So the span has to widen.

How far it widens is a separate question, and "back to the start of the log" is
the wrong default. It loses nothing, but it demands that the entire raw history
fit one summarizer request — which is the request compaction exists because a
conversation cannot make, and the conversations most likely to lose an artifact
are the long ones that have already compacted several times. The repair would
then be rejected on every attempt while materialization kept replaying the same
oversized log: an absorbing state.

An **older checkpoint in the chain** is the way out. Its summary already stands
in for everything up to its own boundary, so rebuilding from there covers
exactly the same history for the price of the span since that boundary. So the
repair walks the chain newest-first and rebuilds from the first ancestor whose
summary still reads. The new checkpoint links to that ancestor, not to the
checkpoint that could not be read; the broken one is unreachable from what the
new summary contains. Only when no ancestor reads either is the whole log the
span, and then it is the last resort rather than the first.

**The walk is not bounded**, and the first version of it was. A fixed window is
the obvious way to cap the artifact reads, and it recreates this same absorbing
state in miniature: the newest N summaries unreadable and an older one intact
means giving up on a chain that had an answer in it. Walking further costs one
failed artifact read per checkpoint, on a path that only runs when a summary has
already been lost, and it stops at the first one that reads.

Either way the span is wider than the prompt the summary model was chosen
against, so a widened rebuild goes to the **agent's** model: it costs more per
token and cannot be the reason the repair fails.

A compaction that finishes summarizing only to find a **newer checkpoint** at
the head stands down without publishing. Everything in its payload — the chain
link, the cumulative `compactedEventCount`, the cut boundary — was computed
against the head as it stood when the pass began, and readers always take the
newest checkpoint. Publishing anyway would let a shorter prefix silently replace
a longer one and leave the chain pointing past a checkpoint no longer reachable
from the head. The handle API has no compare-and-append, so the re-read
immediately before the write narrows the window rather than closing it;
discarding a summary already paid for is the cheap side of that trade.

The summarizer is a real, billable call, so its usage is recorded — on its own
custom event, `exo.compaction.usage.v1`.

**Not on a `messages` event**, and the reason is the same rule as above. Both
materializers treat every messages event as a turn boundary and flush pending
tool calls at it, so an accounting event that landed between a `tool_requested`
and its `tool_result` would make them fabricate a failure for a call that
succeeded and then append the real result too.

Writing it "at a safe moment" is not a fix. Turns on one conversation are not
serialized, so "no tool call is outstanding" is a claim about _every_ in-flight
turn, not just the one doing the accounting — and this bug survived two attempts
that reasoned about timing. A custom event is ignored by prompt assembly
outright, so no ordering rule remains to get wrong in any interleaving.

The cost of that choice is that spend aggregation has to look in two places:
`crates/cli/src/tui.rs` sums both event kinds, and the
`list_conversation_events` description tells the agent to do the same. Miss
either and totals understate by exactly the cost of keeping conversations
compact.

The summarizer request also carries a `max_output_tokens` derived from
`maxSummaryChars`. `capSummary` truncates only _after_ a response is generated,
transferred and billed, so on its own it bounds the stored summary but not what
producing it costs.

That bound is **clamped to the model's own output ceiling**, which is a different
number from its input window — 200k in and 8k out is an ordinary shape, and the
price table carries both. The derived bound deliberately leaves headroom (one
token per character, so a compliant CJK summary is never clipped), and that
headroom is exactly what makes the clamp necessary: providers that validate the
field reject an over-large request rather than trimming it, so the default 8000
sent to a 4k-output summary model fails _every_ summarizer call. Nothing is ever
checkpointed and the conversation reaches the agent model's input wall with
compaction enabled and silently unable to run. An unknown ceiling means no clamp
— the price table is best-effort, and refusing to summarize for an unlisted model
would be the same outage by another route.

## Caching

`BasicExecutor` keeps an incremental history cache, and the TypeScript turn loop
now has the equivalent (`PromptHistoryCache`) — the loop materializes on every
round, so re-reading the whole log each time made a turn cost
O(rounds × events).

**Compaction must invalidate that cache.** It replaces precisely the prefix the
cache holds; a stale cache would keep serving pre-compaction history from memory,
the prompt would never shrink, and nothing would error. Both implementations have
a test for this, and stubbing out the invalidation makes it fail.

**Invalidation alone is not sufficient**, for two separate reasons.

First, the cache must re-read the active checkpoint on _every_ materialization,
not only when priming. Invalidation reaches the cache of whoever compacted;
a checkpoint written by another executor instance, or by the other runtime over
the same conversation, reaches nothing — and the incremental query filters custom
events out, so a warm entry would never see it and would replay the compacted
prefix indefinitely. Both runtimes therefore track which checkpoint an entry was
built against and rebuild when it changes. The cost is one bounded `desc limit 1`
query per round.

Second, turns on one conversation are not serialized. A turn that read a pre-checkpoint entry and then blocked in
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

| Field                | Default       | Meaning                                                    |
| -------------------- | ------------- | ---------------------------------------------------------- |
| `enabled`            | `true`        | Off means unbounded prompts.                               |
| `thresholdRatio`     | `0.7`         | Fraction of the input limit that triggers compaction.      |
| `keepRecentTurns`    | `3`           | Turns kept verbatim after the cut.                         |
| `maxSummaryChars`    | `8000`        | Hard ceiling on summary size.                              |
| `summaryModel`       | agent's model | Model id (within the agent's binding) used for summaries.  |
| `fallbackCharBudget` | `64000`       | UTF-8 bytes. Used when the model's input limit is unknown. |

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

**Unless it is a rescue.** That guard prices the summary at the configured
ceiling, which is the right question while compaction is housekeeping: a cut
reclaiming less than a summary's worth should wait and batch rather than pay per
turn for a sliver. It is the wrong question once the prompt is past the model's
hard input limit. The ceiling is a cap, not a forecast — a concise summary of a
small prefix can be a fraction of it — and the alternative to a small shrink
there is a rejected request. That produces no usage, so the accurate trigger
never runs; no turn completes, so the prefix cannot grow; and the skip would
hold forever. So the trigger tells compaction which of the two it is
(`PromptPressure::over_input_limit`, `RunCompactionArgs.overInputLimit`), and
only the pre-request trigger can set it — a response that came back proves its
prompt fit. The measured no-growth check still guards the outcome, so the worst
case is one summarizer call whose result is discarded.

Within a turn, further attempts are suppressed only while the newest completed
turn boundary is unchanged. Cuts land only on `turn_ended`, so an unchanged
boundary means a re-scan reaches the same answer, and retrying every round would
re-scan the log and possibly re-run the summarizer for it.

The earlier version of this rule latched permanently on the first attempt,
justified by "no new `turn_ended` appears mid-turn". Turns on one conversation
are not serialized, so that is false: other turns finish while this one loops,
and an attempt that skipped for want of completed turns would otherwise suppress
every later check while the prompt kept growing. The latch therefore records the
boundary it last saw rather than a boolean — it stores _why_ re-attempting would
be pointless instead of asserting that it is.

## Known limits

- Compaction bounds the _prompt_, not the _read_.
  `crates/exoharness/src/storage.rs` still loads every event file from disk
  before filtering, so a cursor query remains O(N) at the storage layer.
  Indexing the event log is the complementary fix.
- A conversation whose _retained_ turns alone exceed the input limit cannot be
  rescued by compaction: there is nothing safe left to cut. Lowering
  `keepRecentTurns` helps; cutting mid-turn would corrupt tool rounds.
- A span too large for the summarizer's own context window is not handled.
  Falling back from `summaryModel` to the agent's model covers the
  configuration case; a span that fits no available window would need the
  summarizer to work in chunks.
- Two compactions racing on one conversation resolve by the later one standing
  down, but only because it re-reads the head just before writing. Without a
  compare-and-append primitive there is still a window where both publish.
- The prompt-size estimate is not a tokenizer. It is deliberately conservative
  and script-aware, but a fixed byte-per-token ratio cannot be right for every
  script at once; the provider's own count is the accurate mechanism.
- Summaries are flat, not hierarchical. If a flat summary proves lossy in
  practice, tiered summaries are the next step.
- The summarizer call is a real model round and goes through
  `complete_model_round`, so it is traced and its usage names the model it
  actually asked for. Building its request by hand is how it lost both.
- Summary _quality_ is not covered by the test suite — the deterministic tests
  inject a fake summarizer. A fact-retention eval (plant facts before the cut,
  check recall after) belongs in Braintrust, which is already wired through
  every runtime.
