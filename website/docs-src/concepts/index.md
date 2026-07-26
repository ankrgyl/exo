---
title: Concepts
description: The three core concepts — exoharness, executor, sandbox — and how an agent spans them.
---

# Concepts

![Exo architecture overview](/images/architecture-overview.svg)

Exo is built from three core concepts, each with a different mutability
contract:

## Exoharness — durable state

The trusted substrate. It owns conversation history (the append-only event
log), artifacts, bindings, secrets, and sandbox lifecycle. It is the one
layer the agent **cannot alter**: history can only be appended to, never
rewritten, which is what makes everything else safe to change.

## Executor — policy and tooling

The layer that decides how the agent thinks and acts: prompt assembly,
model calls, tool dispatch, memory and compaction. It is **fully editable
and evolvable** — swappable wholesale (Codex, Claude Code, Cursor SDK, your
own), and modifiable by the agent itself, because nothing durable lives in
it.

## Sandbox — the environment

Where the agent's work actually runs: an isolated machine where it
installs packages, executes commands, and experiments. It is
**checkpointable and rewindable** — snapshots are recorded in the event
log, so environment state can travel with conversation state.

## An agent spans all three

```text
agent = conversation history        (durable state)
      + executor policy & tooling   (fully editable / evolvable)
      + sandboxed environment(s)    (checkpoint & rewindable)
```

This decomposition is the whole design: everything an agent might want to
change about itself lives in the two mutable layers, while the record of
what happened lives in the immutable one. A failed experiment in policy or
environment is always recoverable, because the history that defines the
agent was never at risk.

## Going deeper

- [Exoharness & Executor](./exoharness-and-executor) — why the split
  exists and what it buys you.
- [Data Model](./data-model) — agents, conversations, sessions, turns,
  events, and artifacts.
- [Time Travel](./time-travel) — fork and rewind from any point in the
  event log.
- [Sandboxes](./sandboxes) — backends, scope, and snapshots.
- [Bindings & Secrets](./bindings-and-secrets) — how credentials stay
  out of the model's reach.
- [Executors & Harnesses](./executors) — the built-in executor
  runtimes; a *harness* is an executor running on an exoharness.
- [Tools](./tools) — functions the model calls during a turn, and where
  they come from and run.
- [Adapters](./adapters) — long-running connections to external
  channels like ExoChat, WhatsApp, Signal, Discord, and IRC, and how to
  configure each one.
- [Task Scheduler](./task-scheduler) — recurring sandbox work that wakes
  the conversation with each run's result.
- [The Canonical Agent](./canonical-agent) — what `setup.sh` runs:
  guardian, scheduler, memory, and the self-improvement loop.

The canonical reference is
[`docs/spec.md`](https://github.com/exoharness/exo/blob/main/docs/spec.md);
these pages are a guided tour of the same material.
