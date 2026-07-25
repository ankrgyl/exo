---
title: Executors & Harnesses
description: The built-in executor runtimes, from basic to Codex, Claude Code, and Cursor.
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
| `<module.ts>` | Any TypeScript module path implementing the harness interface |

## The executor loop

Whatever the runtime, the canonical loop is the same:

1. Resolve the effective config, visible bindings, and executor source identity
2. Ensure a content-addressed execution epoch
3. `beginTurn(...)` with the observed agent head and epoch id
4. Read or derive prompt history from events
5. Call the model
6. Append messages and tool requests through the turn handle
7. Execute tools and append results through the turn handle
8. `finish()`

Everything in steps 2–5 is executor policy: which slice of history to
send, which model to call, which tools to expose, when to compact. The
exoharness can even be exposed *to the model* — e.g. a tool for querying
the agent's own history — but that exposure is still configured by the
executor.

The epoch and agent head make executor policy changes explicit in durable
history. See [Causal Execution](./causal-execution).

## TypeScript harnesses

The `typescript` harness runs a module that owns the turn loop:

```bash
exo --harness typescript agent create "TS Basic" \
  --module examples/typescript/basic-harness.ts \
  --model gpt-5.5
```

This is the main extension point for building your own agent — see the
[Tutorials](../tutorials/index) section.

## Coding-agent harnesses

The `codex`, `claude-code`, and `cursor` harnesses treat exoharness events
as the canonical conversation state and run the native agent runtimes
inside exoharness-managed sandboxes. The payoff: sessions you can stop,
resume, fork, and rewind across runs, regardless of which coding agent is
driving.
