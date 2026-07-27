# Adapter Architecture

This document is a review map for the Exo adapter changes. It focuses on the minimal architecture: what owns adapter state, what starts workers, how messages move, and which files to inspect.

## What Adapters Are

Adapters are long-running host-managed connections to external services. They let Exo receive messages from outside the REPL and send explicit replies back out.

The adapter subsystem is intentionally separate from normal tools:

- Tools run during a model turn.
- Adapters run continuously in a background host process.
- Adapter events wake a conversation by creating a normal Exo turn.
- Outbound adapter sends are explicit tool calls, not implicit model output.

## Sources

Adapter records have a `source` describing where the adapter comes from:

Current sources:

- `built_in`: core Exo adapter. IRC is the only built-in adapter.
- `library`: reusable adapter shipped with Exo. Signal and WhatsApp are library adapters backed by shipped workers.

All adapters in this PR are worker adapters: supervised processes using JSONL over stdin/stdout. Protocol-specific code should live under `exo/adapters/<adapter>/`, not in the shared Rust runtime.

## Data Model

Core records live in `crates/executor/src/adapter/types.rs`.

Important types:

- `AdapterRecord`: durable adapter config and status.
- `AdapterConfig::Worker`: worker command, initialization JSON, capabilities, optional state dir, optional secret env vars.
- `AdapterEventRecord`: lightweight event history.
- `AdapterOutboundMessageRecord`: queued outbound messages.

There is no module adapter path in this PR. If agent-authored adapters are added later, they should compile or resolve to the same worker shape.

## Storage

The adapter store is file-backed in `crates/executor/src/adapter/store.rs`.

Default root:

```text
.exo/adapters/
```

Layout:

```text
.exo/adapters/adapters/<adapter-id>.json
.exo/adapters/events/<adapter-id>/<event-id>.json
.exo/adapters/outbox/<adapter-id>/<message-id>.json
.exo/adapters/<adapter-type>/<adapter-id>/...
```

Adapter records and event records stay in the store. Larger, conversation-visible payloads are written as conversation artifacts by the runtime.

## Runtime Ownership

The adapter runner is a host process started by the Exo script:

```text
exo/scripts/exo-repl
```

It starts:

```bash
exo --harness exo adapters run --watch --limit <N>
```

The CLI entry point is:

```text
crates/cli/src/adapters.rs
```

Responsibilities:

- Acquire a lock so only one adapter watch runner is active.
- Dispatch `adapters list`, `adapters run`, `adapters disable`, and `adapters delete`.
- Call the executor adapter runtime.

The watch loop is in:

```text
crates/executor/src/adapter/runtime.rs
```

Responsibilities:

- Poll enabled adapter records.
- Start one supervisor task per enabled adapter.
- Skip adapters that are disabled or not build-ready.
- Restart workers after they exit or error.
- Convert worker events into store records, artifacts, and conversation wakeups.
- Drain the outbox and write outbound commands to workers.

## Worker Protocol

The shared worker protocol is implemented in Rust and mirrored in TypeScript:

```text
crates/executor/src/adapter/worker.rs
exo/adapters/protocol.ts
```

Host to worker:

```json
{ "type": "send_message", "target": "...", "text": "..." }
```

Worker to host:

```json
{"type":"connected","subject":"...","metadata":{}}
{"type":"message","target":"...","sender":"...","text":"...","message_id":"...","metadata":{}}
{"type":"lifecycle","name":"...","metadata":{}}
{"type":"error","message":"..."}
{"type":"disconnected","reason":"..."}
```

Workers receive configuration via environment:

- `EXO_ADAPTER_ID`
- `EXO_ADAPTER_TYPE`
- `EXO_ADAPTER_STATE_DIR`
- `EXO_ADAPTER_CONFIG`
- protocol-specific secret env vars, such as `EXO_IRC_PASSWORD`

## Inbound Flow

1. A worker receives an external message.
2. The worker writes a `message` JSONL event to stdout.
3. `run_worker_loop` parses it.
4. `runtime.rs` writes an inbound artifact into the owning conversation.
5. `runtime.rs` records a store event.
6. `runtime.rs` calls `send_conversation_wakeup`.
7. Exo receives a normal user message containing:
   - adapter name
   - adapter id
   - target
   - sender
   - message text
   - instructions for replying with `send_adapter_message`

The wakeup path is shared with scheduler wakeups:

```text
crates/executor/src/conversation_wakeup.rs
```

## Outbound Flow

1. The model explicitly calls `send_adapter_message`.
2. TypeScript tool definitions pass the request to the host tool runtime.
3. `runtime.rs` writes an outbound artifact into the conversation.
4. `AdapterStore` writes an outbox record.
5. The adapter runner drains the outbox once per second.
6. The host writes a `send_message` JSONL command to the worker stdin.
7. The worker sends through the external protocol.

This avoids short-lived reconnects for every outbound message.

## Tool Integration

Model-facing adapter tools are defined in:

```text
exoharness/typescript/harness/adapter-tools.ts
```

Tools:

- `create_adapter`
- `list_adapters`
- `disable_adapter`
- `delete_adapter`
- `send_adapter_message`

These tools are registered by the Exo harness:

```text
exo/harness.ts
```

Host-side execution is in:

```text
crates/executor/src/harness_tool.rs
crates/executor/src/adapter/tools.rs
```

The TypeScript layer currently transforms typed user-facing adapter configs into generic worker configs. For example, a Signal config becomes a worker config pointing at:

```text
exo/adapters/signal/worker.ts
```

## Protocol Workers

Protocol-specific code lives under:

```text
exo/adapters/
```

Current workers:

- `irc/worker.ts`: IRC socket, registration, channel join, PING/PONG, PRIVMSG parsing.
- `whatsapp/worker.ts`: Baileys linked-device client, QR pairing, WhatsApp messages.
- `signal/worker.ts`: `signal-cli` linked-device flow, JSON-RPC receive/send.

Each adapter directory also has a local README and setup prompt:

```text
exo/adapters/irc/README.md
exo/adapters/irc/setup-prompt.md
exo/adapters/whatsapp/README.md
exo/adapters/whatsapp/setup-prompt.md
exo/adapters/signal/README.md
exo/adapters/signal/setup-prompt.md
```

## Lifecycle

Adapter lifecycle is owned by the host runner, not by the REPL.

Startup:

- `exo/scripts/exo-repl` starts `exo adapters run --watch` unless `--no-adapters` is set.
- The runner writes `.exo/exo-adapters.pid` and logs to `.exo/exo-adapters.log`.
- The runner starts worker processes for enabled, ready adapters.

Restart:

- Worker exit/error returns from `run_worker_loop`.
- The watch task records the error and retries after a short delay.

Stopping:

- `stop_adapters` in `exo/scripts/exo-repl` kills the runner and worker processes.
- Disabling/deleting adapter records prevents future restarts.

## Files To Inspect For PR Review

Core model and runtime:

- `crates/executor/src/adapter/types.rs`
- `crates/executor/src/adapter/store.rs`
- `crates/executor/src/adapter/runtime.rs`
- `crates/executor/src/adapter/worker.rs`
- `crates/executor/src/adapter/tools.rs`
- `crates/cli/src/adapters.rs`

TypeScript tool surface:

- `exoharness/typescript/harness/adapter-tools.ts`
- `exoharness/typescript/harness/index.test.ts`
- `exo/harness.ts`

Protocol-specific workers:

- `exo/adapters/protocol.ts`
- `exo/adapters/irc/worker.ts`
- `exo/adapters/whatsapp/worker.ts`
- `exo/adapters/signal/worker.ts`

Script and docs:

- `exo/scripts/exo-repl`
- `exo/README.md`
- `exo/docs/adapter-architecture.md`
- `exo/adapters/*/README.md`
- `exo/adapters/*/setup-prompt.md`

## Minimality Notes

The intended split is:

- Rust owns durable records, lifecycle supervision, outbox, artifacts, and conversation wakeups.
- TypeScript harness owns model-facing tool schemas and transforms.
- Adapter directories own protocol-specific code.

For PR cleanup, the main question to ask in each file is whether it belongs to one of those boundaries. Protocol details should not leak into the Rust runtime beyond generic worker configuration.
