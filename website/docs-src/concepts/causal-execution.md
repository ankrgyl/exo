---
title: Causal Execution
description: Reconstruct which agent state and executor version produced every turn.
---

# Causal Execution

A conversation event log can tell you what happened inside a turn. It cannot,
by itself, tell you which shared agent configuration, bindings, harness code,
or deployment produced that turn. Those inputs can change independently of the
conversation.

Exoharness gives every turn three causal coordinates:

```text
conversation position × agent position × execution epoch
```

- The **conversation position** is the turn's position in its append-only
  conversation event log.
- The **agent position** (`agentEventId`) is the latest shared-agent event the
  executor observed before beginning the turn.
- The **execution epoch** (`executionEpochId`) identifies an immutable,
  content-addressed execution manifest.

The `turn_started` event stores the same agent and epoch ids as the
`TurnRecord`, so the coordinates remain part of durable history.

## The agent timeline

Every agent has an append-only event timeline alongside its conversation
timelines. Built-in events cover:

- agent and conversation lifecycle;
- agent artifact versions;
- binding and secret-metadata updates;
- agent-scoped sandbox activity;
- execution-epoch creation and activation;
- namespaced custom events.

Secret values are never written to this timeline. `secret_put` contains only
the public `SecretMetadata`.

Use `Agent.getEvents()`, `getEvent()`, and `addEvents()` to inspect or extend
the timeline. Rust callers can additionally subscribe with `watch_events()`
when the backend supports a live stream.

The CLI prints the same structured records:

```bash
exo agent events <agent>
exo agent events <agent> --type execution_epoch_created --desc --limit 1
exo agent events <agent> --type execution_epoch_activated --desc --limit 1
```

## Execution epochs

Before a built-in executor begins a turn, it builds an execution manifest
containing:

- the executor crate name, version, and selected harness;
- the effective agent and conversation configuration;
- all bindings visible to the conversation;
- metadata for all visible secrets, without secret values;
- SHA-256 hashes of the configured TypeScript harness and direct tool modules.

`ensureExecutionEpoch()` hashes the manifest and resolves it through persisted
digest and id indexes. If it matches the active epoch, the existing epoch is
reused without another event. If it matches an older epoch, that immutable
epoch is reactivated. Only a manifest not seen before creates a new epoch.
Unchanged turns therefore pay the state-read and hashing cost but do not scan
the agent timeline or create duplicate epoch records.

An `execution_epoch_created` event stores the immutable manifest once and makes
that epoch active. Later `execution_epoch_activated` events contain only its id,
so switching back to a known epoch does not duplicate the manifest.

The epoch is a statement about the inputs exoharness can observe. For built-in
Rust executors, code identity is the crate version. For TypeScript executors,
the configured harness and direct tool files are also content-hashed. External
services and transitive package contents should be represented by additional
manifest fields or agent events when exact reconstruction requires them.

## Atomic turn pinning

The executor reads the agent head before assembling the manifest and passes it
as `expectedAgentEventId`. Epoch selection fails closed if shared state changes
during that read. Epoch activation and turn creation are separate writes, so
the executor then passes both returned ids explicitly to `beginTurn()`:

```text
read head → assemble manifest → ensure unchanged head + epoch → begin pinned turn
```

Exoharness validates that the epoch existed at or before the supplied agent
head. A turn cannot claim a future epoch while pinning an older view of agent
state. Callers that omit the ids retain the simpler API: `beginTurn()` captures
the current agent head and active epoch atomically.

This is the foundation for replay, audit, deployment comparisons, and safer
self-modification: a result is no longer attached only to a conversation; it
is attached to the shared state and executor definition that caused it.
