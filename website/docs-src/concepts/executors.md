---
title: Executors & Harnesses
description: The built-in executor runtimes, from basic to Codex, Claude Code, Cursor, and Pi.
---

# Executors & Harnesses

A harness is an executor running on the exoharness. The `exo` CLI selects
one with `--harness`:

| Harness | What it is |
|:--------|:-----------|
| `basic` | Built-in Rust executor: straightforward prompt → model → tools loop |
| `rlm` | Recursive-language-model experiment |
| `typescript` | Runs a TypeScript harness module that owns the turn loop, while Rust owns durable state |
| `codex` | Backs OpenAI Codex with durable exoharness sessions |
| `claude-code` | Backs Claude Code with durable exoharness sessions |
| `cursor` | Backs the Cursor SDK with durable exoharness sessions |
| `pi` | Backs Pi with durable exoharness sessions |
| `<module.ts>` | Any TypeScript module path implementing the harness interface |

## The executor loop

Whatever the runtime, the canonical loop is the same:

1. `beginTurn(...)` — durably accept the user input, get a turn handle
2. Read or derive prompt history from events
3. Call the model
4. Append messages and tool requests through the turn handle
5. Execute tools and append results through the turn handle
6. `finish()`

Everything in steps 2–5 is executor policy: which slice of history to
send, which model to call, which tools to expose, when to compact. The
exoharness can even be exposed *to the model* — e.g. a tool for querying
the agent's own history — but that exposure is still configured by the
executor.

## TypeScript harnesses

The `typescript` harness runs a module that owns the turn loop:

```bash
exo --harness typescript agent create "TS Basic" \
  --module exoharness/examples/typescript/basic-harness.ts \
  --model gpt-5.5
```

This is the main extension point for building your own agent — see the
[Tutorials](../tutorials/index) section.

## Coding-agent harnesses

The `codex`, `claude-code`, `cursor`, and `pi` harnesses treat exoharness
events as the canonical conversation state and run the native agent runtimes
inside exoharness-managed sandboxes. The payoff: sessions you can stop,
resume, fork, and rewind across runs, regardless of which coding agent is
driving.
