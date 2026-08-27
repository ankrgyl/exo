---
title: Exoharness & Executor
description: Why exo splits trusted infrastructure from agent semantics.
---

# Exoharness & Executor

Powerful autonomy requires agents to be two things at once:

1. **Adaptable** — able to change their policies, tools, and architecture to
   fit a domain, including changing *themselves*.
2. **Trustworthy** — durable across crashes, isolated from one another, and
   recoverable to known-good states.

Most agent systems struggle to provide both because they conflate trusted
infrastructure with agent-specific implementation (prompts, memory,
compaction). If the thing that stores your history is the same thing that
decides your prompts, you can't safely let an agent rewrite its prompts —
a bad change can corrupt or lose the history too.

Exo decouples the two into halves:

![Exo architecture, detailed](/images/architecture-detailed.svg)

## The exoharness

The durable substrate. It owns identity (agents, conversations, turns),
history (the append-only event log), artifacts, secrets, and sandbox
management. It is **trusted and stateful** — and deliberately minimal. It
deliberately does *not* call the LLM: that requires semantic choices (how
to stitch the prompt, which model, what approvals) that belong to the
executor, not to trusted infrastructure.

## The executor

The policy layer. It runs the turn loop: assembling prompts, calling
models, dispatching tools, deciding memory and compaction policy, handling
approvals. It is **ephemeral and swappable** — it can be killed, upgraded,
or replaced without losing the agent, because everything durable lives
below it.

## What the split buys you

Because the substrate doesn't depend on the executor, an agent built on exo
can:

- **Fork or rewind** at any past event, without losing secrets, sandboxes,
  or history.
- **Swap executors** — run the same agent via Codex, Claude Code, the
  Cursor SDK, Pi, or your own executor, without rebuilding state.
- **Evolve safely** — change its own policy processes, tools, and even
  harness code, while the exoharness isolates secrets and compute and keeps
  canonical history out of reach.

The point of keeping the exoharness minimal is not architectural
cleanliness. It maximizes the space of behaviors that can evolve *above*
it: the more of the agent's memory, orchestration, and execution strategy
that lives in the swappable layer, the more of the agent can become
programmable and agentic — while the system stays safe and recoverable.
