# Exoharness Server Plan

This is the direction for TypeScript harnesses and multi-client access to the substrate.

## Goal

Keep `exoharness` itself as the substrate API and local implementation, and expose a long-lived local server from it so that:

- Rust harnesses can use the local implementation directly or through a client.
- TypeScript harnesses can use a real `exoharness` client.
- Multiple harnesses and CLI processes can attach to the same substrate at the same time.
- We do not invent a second bespoke "TypeScript harness API" that drifts from the real substrate.

This should feel closer to the `tmux` / `opencode` model:

- one long-lived local backend owns the real state
- clients attach to it
- handles stay server-side
- clients use stable ids / proxy objects

## Non-goals

- Do not rewrite the `exoharness` traits and semantics.
- Do not introduce a separate public "document" abstraction for config.
- Do not build the first version on HTTP unless we specifically need remote access.
- Do not build the first version as a Node native extension.

## Proposed architecture

### Keep

- `crates/exoharness`
  - traits
  - types
  - local backend implementation
  - storage / sandbox semantics

### Add

Initially, these should just be modules inside `crates/exoharness`, not new crates:

- `exoharness::server`
  - wraps an `Arc<dyn ExoHarness>`
  - serves the existing substrate over local IPC
  - owns live handle tables for agent / conversation / turn proxies

- `exoharness::client`
  - Rust client for the IPC protocol
  - same high-level API shape as `ExoHarness`, `AgentHandle`, `ConversationHandle`, `TurnHandle`

- `exoharness::protocol`
  - shared IPC request / response types
  - handle ids and streaming/subscription message types

If this grows enough later, we can split these behind features or separate crates. That should be a packaging decision, not the starting point.

- `typescript-exoharness-client`
  - TypeScript SDK / client with the same high-level concepts
  - proxy objects backed by the IPC protocol

### Transport

Use local IPC:

- Unix domain socket on macOS/Linux
- named pipe on Windows
- JSON-RPC first

Reasons:

- local and stateful
- easy to debug
- easy to inspect with logs
- no port management
- good enough latency for harness operations

If JSON becomes annoying later, we can switch the framing / payload format while preserving the API shape.

## API shape

The key point is that TS should use real `exoharness`-shaped objects, not a custom `HarnessContext`.

### Rust

The server starts with a real `Arc<dyn ExoHarness>`.

The client exposes:

- `ExoHarnessClient`
- `AgentHandleClient`
- `ConversationHandleClient`
- `TurnHandleClient`

These should implement the same traits as closely as possible.

### TypeScript

The TS SDK should expose:

- `ExoHarness`
- `Agent`
- `Conversation`
- `Turn`

with methods that mirror the substrate:

- root:
  - `listAgents`
  - `getAgent`
  - `newAgent`
  - `deleteAgent`

- agent:
  - `listConversations`
  - `getConversation`
  - `newConversation`
  - `deleteConversation`
  - `listArtifacts`
  - `readArtifact`
  - `writeArtifact`
  - bindings / secrets later

- conversation:
  - `startSession`
  - `endSession`
  - `beginTurn`
  - `getEvents`
  - `getEvent`
  - `addEvents`
  - `fork`
  - `listArtifacts`
  - `readArtifact`
  - `writeArtifact`
  - `watchEvents` later
  - sandboxes later

- turn:
  - `addEvents`
  - `finish`

The TS SDK can add convenience helpers on top, such as:

- `readText`
- `readJson`
- `writeText`
- `writeJson`

but these are helpers over artifacts, not a new substrate abstraction.

## Handle model

Trait objects do not cross IPC. The server should therefore:

- keep the real live handles in a server-side registry
- hand out opaque handle ids to clients

The protocol should distinguish:

- stable ids already in the substrate:
  - `agent_id`
  - `conversation_id`
  - `turn_id`
  - `event_id`
  - `artifact_id`

- ephemeral proxy ids for live RPC objects where needed

For most operations:

- `Agent` and `Conversation` can be addressed by real ids
- `Turn` is where a server-side live handle is most useful

## TypeScript harness direction

TypeScript harnesses should be built on the exoharness client, not on a special-purpose executor bridge.

That means:

- TS "basic harness" becomes:
  - exoharness client
  - model client
  - tool runtime

- TS "memory" or "forking" features use the same substrate primitives Rust does

- a TS harness author should be able to write code against:
  - conversations
  - turns
  - events
  - artifacts
  - forks

without learning a second host-specific API

## What to delete from the current TypeScript spike

The following code is the wrong long-term shape and should be removed once the IPC-backed exoharness client exists:

- the bespoke TypeScript executor bridge in:
  - `crates/executor/src/typescript.rs`

- the custom host request protocol in:
  - `exoharness/typescript/harness/runner.ts`

- the bespoke `HarnessContext` surface in:
  - `exoharness/typescript/harness/index.ts`

This code is useful as a spike because it proved:

- TS harnesses are viable
- the CLI/config wiring works
- authoring a harness in TS is reasonable

But it should not be the final architecture.

## What to preserve from the current TypeScript spike

These ideas are still good and should survive the refactor:

- `--harness typescript`
- `--module <path>`
- typed TS helpers around messages / artifacts
- example harnesses
- keeping TS harness authoring ergonomic and JS-native

The ergonomic helpers should be rebuilt on top of the exoharness client rather than the current bespoke host bridge.

## Relationship to the executor crate

After this refactor:

- Rust `basic` and `rlm` executors remain in `crates/executor`
- they keep using `exoharness`
- TypeScript harnesses do not need to be modeled as a special Rust executor

Instead:

- the CLI resolves the agent config
- if the harness kind is `typescript`, it launches the TS harness runtime with an exoharness client connection
- the TS harness talks to the same substrate service as everything else

So the Rust side stops pretending that a TS harness is a normal Rust executor implementation.

## Migration plan

### Phase 1: server and Rust client

1. Add `server`, `client`, and `protocol` modules under `crates/exoharness`.
2. Add a local IPC transport.
3. Validate that a Rust CLI process can talk to the server instead of the in-process backend.

### Phase 2: TypeScript client

1. Build a TS SDK that talks to the same IPC protocol.
2. Expose real `ExoHarness` / `Agent` / `Conversation` / `Turn` proxies.
3. Add ergonomic artifact helpers (`readJson`, `writeJson`, etc.).

### Phase 3: TS harness runtime

1. Replace the current bespoke `HarnessContext` runtime with a smaller launcher that gives the TS harness an `ExoHarness` client.
2. Rebuild the example harness on top of the exoharness client.

### Phase 4: delete the spike

Remove:

- `crates/executor/src/typescript.rs`
- the bespoke JSONL host request protocol
- the bespoke `HarnessContext` SDK

Keep only the TS harness launcher and the exoharness TS client.

## Immediate code cleanup implied by this direction

If we commit to this design, we should expect to delete most of the current TypeScript-specific Rust glue rather than polish it further.

Concretely, that means:

- do not keep adding features to `TypeScriptExecutor`
- do not keep expanding the bespoke `HarnessContext`
- do not keep moving more substrate methods into ad hoc host request enums

That code should be treated as temporary scaffolding.

## Summary

The correct long-term shape is:

- keep `exoharness` as the substrate
- expose a real local server from it
- let Rust and TypeScript both connect as clients
- make TypeScript harnesses use real exoharness primitives
- delete the current bespoke TS bridge once the real client exists
