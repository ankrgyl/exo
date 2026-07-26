---
title: exo
description: A systems approach to recursive self-improvement.
---

# exo

Exo is a complete agent harness with tools, scheduled tasks, adapters
(ExoChat, WhatsApp, Discord, Signal, IRC), computer use, and state management
(snapshot, clone, migrate, rewind). Most importantly, an Exo agent has full
visibility into its own code and runtime logs, and can incrementally improve
every aspect of itself.

Many agents support some form of self-improvement — evolving prompts, adding
tools. Exo is *fully recursive*: the agent can operate on any layer of
itself, from prompts to memory to tooling to the harness code it runs on.
The architecture constrains this evolution to be incremental and
recoverable: the agent modifies itself above a trusted substrate it cannot
alter. That substrate's append-only event log records canonical history, so
any change — successful or not — can be inspected, rewound, or forked from.

- [**Getting Started**](/getting-started/) — One setup script to a running agent you can chat with from your browser.
- [**Concepts**](/concepts/) — The architecture: exoharness, executors, events, sandboxes, and time travel.
- [**Tutorials**](/tutorials/) — Build your own harness with custom tools and context.
- [**Discord**](https://discord.gg/8x23hdBJU6) — Join the community for help and discussion.
## How it stays safe

Exo splits an agent into two halves:

- **exoharness** — the durable, trusted substrate. Owns identity, the
  append-only event log, artifacts, secrets, and sandbox lifecycle.
- **executor** — the swappable policy layer. Owns prompt assembly, model
  calls, tool dispatch, memory, and the turn loop.

Everything the agent can modify lives *above* the substrate. That's why an
Exo agent can rewrite its own tools, prompts, and harness code, then fork or
rewind to a known-good state if an experiment goes wrong — without ever
losing secrets, history, or sandboxes.

::: warning
  Exo is early software. The public API should be treated as unstable.
:::
