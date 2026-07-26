# Cross-runtime fixtures

Golden files that pin a wire format both the Rust and TypeScript runtimes must
agree on. Each fixture is parsed by tests in _both_ languages, so a change to one
side that the other does not follow fails the build instead of silently breaking
interoperability at runtime.

## `compaction-checkpoint.json`

One compaction checkpoint event, as it appears in a conversation's durable log.

The load-bearing part is the **envelope**, not the payload. Custom events travel
as `{ type: "custom", event_type, payload }` — `Custom` is the only extensible
variant of Rust's `EventData` enum (`crates/exoharness/src/types.rs`), which is
`#[serde(tag = "type")]`, so any other `type` value is rejected as an unknown
variant. TypeScript's `EventData` is `{ type: string } & Record<string, unknown>`
and cannot catch that mismatch at compile time.

Both runtimes shipped a version of this feature that disagreed here: Rust wrote
the envelope and TypeScript wrote a flattened `{ type: "exo.compaction.v1", ...payload }`.
Each side's own tests passed, because each side's tests encoded the same
assumption as its implementation. This fixture is the arbiter neither side owns.

Read by:

- `crates/executor/src/compaction.rs` (tests) — deserializes into `EventData`
  and then `CompactionCheckpoint`.
- `typescript/harness/compaction.test.ts` — decodes via `checkpointFromEvent`.
