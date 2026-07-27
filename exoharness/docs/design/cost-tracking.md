# Per-Message Cost, Token, and Latency Tracking

Every LLM response in exo carries a durable, per-message record of token usage
and timing in the canonical event log. **Dollar cost is a policy computed in
userspace, not by the trusted substrate** — the store persists whatever numbers
the event carries. Cost computation is an agent-side policy, so should not live
in the exoharness.

This document describes what is recorded, who computes cost and where, and the
reasoning behind that split.

## Design Principles

- **Cost is policy, and policy lives in userspace.** How to turn tokens into
  dollars (which price table, which provider formula, when to compute) is an
  application decision, not a substrate responsibility. To not force each substrate
  to reimplement, we provide a standalone cost library that executors call.
- **The substrate stays minimal.** `exoharness` stores the usage record verbatim,
  including an optional `cost_usd` field, but contains no pricing code and never
  computes, validates, or owns cost. It round-trips bytes.
- **Usage is agent-reported telemetry, not an attested ledger.** The token counts
  come from the provider response, which for the TypeScript/exo path is read
  _inside the agent-side harness process_ (the model call is not made by the
  trusted Rust core — there is no model-completion runtime request). So usage is
  self-reported wherever cost is later computed. The implication is that it cannot
  be fully trustworthy: the agent owns its own calls so can report whatever numbers
  it wants.
- **A maintained data source, not a hand-rolled table.** Prices change often and
  across many providers, so the _data_ is the community-maintained LiteLLM price
  database — one source of truth, shared by every consumer of the cost library.
- **Backward and forward compatible.** The usage record and all sub-fields are
  optional; legacy events without `usage` still deserialize, and `cost_usd` may
  simply be null when no policy filled it.

## What Is Recorded

`EventData::Messages` gains an optional `usage` field holding a `UsageRecord`
(the type is event schema and lives in `exoharness`; the store treats it as data):

| Field                          | Meaning                                                                |
| ------------------------------ | ---------------------------------------------------------------------- |
| `model`                        | Model id echoed by the provider, falling back to the requested binding |
| `prompt_tokens`                | Input tokens (provider convention varies — see per-provider math)      |
| `completion_tokens`            | Output tokens                                                          |
| `prompt_cached_tokens`         | Cache-read (discounted) input tokens                                   |
| `prompt_cache_creation_tokens` | Cache-write input tokens (Anthropic)                                   |
| `completion_reasoning_tokens`  | Reasoning tokens, when surfaced                                        |
| `cost_usd`                     | USD cost filled by userspace policy; null if no policy computed it     |
| `ttft_ms`                      | Time to first token (streaming path only)                              |
| `duration_ms`                  | Wall-clock duration, request start to end of response                  |

Every sub-field is `Option` with `skip_serializing_if`. The record is boxed
inside `EventData` (`Option<Box<UsageRecord>>`) to keep the enum small and dodge
`large_enum_variant`; `Box` is serde-transparent, so the on-disk JSON is
unchanged. The two cache buckets are both kept because reads and writes bill at
different rates — cost can't be reconstructed from a single collapsed number.
They mirror lingua's `UniversalUsage` field names.

## Layering: Who Computes Cost

```
TRUSTED SUBSTRATE                    USERSPACE (agent-side)
─────────────────                    ──────────────────────
exoharness store                     Rust executors (Basic/RLM) ─┐
  • persists UsageRecord verbatim                                 ├─ cost library
  • raw tokens + model + timing      TS harness / exo ───────┘   (policy)
  • cost_usd is just a field;                  │
    NOTHING here computes it                   └─ fills cost_usd when building the
  • no pricing dependency                         messages event (or leaves it null
                                                  for a reader to compute later)
```

- The emitter (whoever made the call and holds the provider usage) builds the
  `UsageRecord` and, as a policy choice, fills `cost_usd` by calling the cost
  library. Rust executors call the Rust crate; the TS harness calls the TS port.
- The substrate stores the record as-is. It has no pricing code and no dependency
  on the cost library.
- _When_ cost is computed is an application choice: at write (emitter fills it) or
  left null for a read-time/report path. The substrate is agnostic either way.

## The Cost Library

There are two userspaces (Rust executors, the TS harness), so the cost library
exists once per language. Each is **self-contained** — it owns both the math and
its own data loading, and neither depends on the other having run:

- **Rust:** a `cost` crate (price table, lookup, `compute_cost_usd`, loader), used
  by the Basic executor. The CLI loads the table once at startup (`--pricing-path`
  / `--pricing-url`, `EXO_LITELLM_PRICES_*` as env) and injects it. `exoharness`
  does not depend on it.
- **TypeScript:** a self-contained port (`@exo/model-runtime/cost`) used by the
  exo harness. It loads its own data through the harness's normal config flow
  — reading `EXO_LITELLM_PRICES_PATH` from its inherited env, then its own cache,
  then its own fetch — and computes cost when building the messages event. It does
  **not** depend on the Rust loader having populated anything.

Only the price _rates_ are a single source of truth (the upstream LiteLLM JSON);
the short per-provider formula and the loader logic are duplicated per language.
That duplication is the accepted cost of keeping cost a per-userspace policy
rather than a substrate service, and of each userspace owning its own data so
there is no cross-boundary coupling.

### Loading the data

Each loader resolves the table once and holds it as plain data, in this order:
explicit `EXO_LITELLM_PRICES_PATH` (or `--pricing-path` on the CLI) → fresh
on-disk cache (`$XDG_CACHE_HOME/exo/litellm_prices.json`, 24h TTL) → HTTP fetch
(`EXO_LITELLM_PRICES_URL` or the LiteLLM default, cached on success) → stale cache
→ none. A corrupt cache or unparseable fetch degrades to no table (cost stays
null, tokens persist); the cache is written only when a fetched body parses. The
two loaders share the same cache _path_ by convention, but neither requires the
other to have written it.

### Per-provider cost math

Providers disagree on what `prompt_tokens` _includes_, and getting it wrong
distorts cost by up to ~10× on cache-heavy requests:

- **Anthropic-family** (`anthropic`, `vertex_ai-anthropic`, `azure_ai`):
  `prompt_tokens` is **fresh** input only; cache reads and writes are separate and
  additive — `prompt·in + cached·cache_read + cache_creation·cache_write + completion·out`.
- **Everything else** (`openai`, `mistral`, Bedrock, …): `prompt_tokens` is the
  **total** input; cached is a subset to subtract first —
  `(prompt − cached)·in + cached·cache_read + completion·out`.

The additive set is a narrow list of first-party Anthropic providers; everything
else, including all of Bedrock, is treated as inclusive. Bedrock is not fully
handled — Bedrock-hosted Claude is really additive, so its cache-heavy costs are
off. Left as a TODO. Unclassified providers default to inclusive (safe when
`cached == 0`, the common case).

### Model lookup

Exact match first, then the longest entry key that is a prefix of the requested
model **at a token boundary** — the next character must be absent or a separator
(`-` / `:`). This resolves dated revisions (`claude-sonnet-4-6-20251022` →
`claude-sonnet-4-6`) while refusing to slide `gpt-4o-mini` onto a `gpt-4` entry
when `gpt-4o` is missing, so a model is never silently priced at a neighbor's
rate.

## Testing

- Cost library (Rust) unit tests: Anthropic additive (no cache / hits / creation),
  inclusive (with/without cache, subtraction asserted), boundary-aware lookup
  (incl. the `gpt-4o-mini` vs `gpt-4` non-match), `sample_spec` skip,
  unknown-model null, provider classification (Bedrock → inclusive), and the
  corrupt-cache-degrades-to-empty loader path.
- TS cost port: a parity unit test over the same fixtures so the two formulas
  stay in agreement.
- Executors: assert the persisted `Messages` event carries the expected cost for
  the Anthropic and inclusive paths, through a Rust executor and through the
  exo/TS path, so coverage is pinned on both userspaces.
- `server_duration_ms` removed; the legacy-no-`usage` backward-compat test stays.

## Not in Scope (Intentional)

- **Trusted/attested usage.** Usage is agent-reported (see principles); making it
  tamper-evident means routing model calls through the Rust core — a separate
  change.
- **`/cost` REPL surface.** Data is persisted; the UI is a follow-up. (It is one
  natural read-time consumer of the cost library.)
- **Cache-tier resolution.** lingua's `UniversalUsage` collapses Anthropic's
  5-minute and 1-hour cache writes, so cost uses the 5-minute rate;
  `cache_creation_input_token_cost_above_1hr` is parsed but unused.
- **Long-running staleness.** The data loads once at startup; a long-lived service
  would need a periodic refresh.
- **Anthropic aggregate Admin API.** Org-level, aggregate-only; can't be tied to a
  message.
