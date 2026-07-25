---
title: Time Travel
description: Fork and rewind agents from any point in the event log.
---

# Time Travel

At any point in time, the durable state of an agent is defined by an agent
timeline position plus positions in its conversation logs. A turn also pins
the [execution epoch](./causal-execution) that interpreted that state. Those
coordinates are what make time travel possible: you can **rewind or fork from
a known point**, and identify the configuration and executor that produced
the original result.

## Rewind and fork

- **Rewind** returns a conversation to a known-good earlier state, without
  losing secrets, bindings, or the ability to inspect what happened after
  (the log is append-only; history isn't destroyed).
- **Fork** branches a new conversation from an existing one:

```bash
exo conversation fork <agent> <conversation> "Fork Name"
```

The data model supports recreating state as of *any* past event; the CLI
currently exposes forking at the conversation level.

## Sandboxes travel too

Sandboxes can be snapshotted, which writes a snapshot id into the event
log. An executor can snapshot after every action, or let the LLM decide
when. Because snapshots live in the log, rewinding a conversation can also
rewind its sandbox to the matching filesystem state.

## Why this matters for self-modifying agents

An agent that experiments on itself — editing its tools, prompts, or
harness code — needs an undo button that it *cannot* break. Because the
event log lives in the trusted exoharness, below everything the agent can
touch, a failed experiment is always recoverable: rewind the sandbox,
fork from before the change, and the canonical history of what went wrong
is still there to learn from.
