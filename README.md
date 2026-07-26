<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/exo_badge_dark.svg">
    <img src="docs/images/exo_badge_light.svg" alt="exo logo" width="160" />
  </picture>

# exo

[![CI](https://github.com/exoharness/exo/actions/workflows/ci.yml/badge.svg)](https://github.com/exoharness/exo/actions/workflows/ci.yml)
[![Integration tests](https://github.com/exoharness/exo/actions/workflows/integration.yml/badge.svg)](https://github.com/exoharness/exo/actions/workflows/integration.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg?logo=rust)](Cargo.toml)
[![TypeScript](https://img.shields.io/badge/typescript-5.x-3178c6.svg?logo=typescript&logoColor=white)](tsconfig.json)

[Documentation](https://exoharness.ai/docs) · [Examples](examples/) · [Discord](https://discord.gg/8x23hdBJU6)

</div>

Exo is a systems approach to recursive self improvement. In short, it's a
complete AI agent harness (supporting tools, tasks, integrations, etc.
similar to OpenClaw, Pi or Hermes), with the crucial difference that it
has full visibility into both its code and runtime logs. This allows Exo
to incrementally improve every aspect of itself, clone itself, and even
manage a lineage of clones.

While most agents can do some form of self improvement, such as updating
memory or creating skills, Exo is fully recursive in that it can clone or
operate on any aspect of itself, from prompts, to memory, tooling, or even
basic harness policy itself. It's architected so that this evolution can
be done incrementally
and (mostly) safely. The only thing it can't muck with is an event log which
provides a canonical history of what it's tried to prevent getting stuck in
recursive loops.

The goal of Exo is to be the minimal framework possible to give an agent full
ability for recursive self improvement. Why would you want such a thing?

- It's a good agent framework to build exactly the agent you like, that's
  maximally Bitter Lesson aligned. Future smarter models can evolve every aspect
  of the system at runtime, and do it safely with full history.
- It's a good framework to allow AI models to solve complex problems by
  iterating on system level properties. We've had agents learn to play games,
  cost-optimize themselves, build complex systems. In each case, it required the
  agents to modify themselves heavily beyond memory.

In short, we think this is the best way to take advantage of the growing power
of AI models when building long-lived agents.

For a more complete description of the architectural philosophy read
[A Systems View of Recursive Self Improvement](docs/RSI.md)

<!-- ![Exo playinb pokemon go](docs/images/exo_playing.gif) -->

## Quick Start

Exo was designed to be incredibly simple to use. With just a few commands you
should have a fully functional agent who can do standard agent tasks (computer
use, research, coding etc.) but can also extend itself as needed.

To use Exo as an agent, you'll need an OpenAI or OpenRouter API key. If you
have that, simply do the following:

```
curl -fsSL https://raw.githubusercontent.com/exoharness/exo/main/setup.sh -o setup.sh
bash setup.sh
```

_Note that Exo requires git and Docker. The setup script offers to install
them if missing, and installs pinned node, pnpm, and rust toolchains
automatically via [mise](https://mise.jdx.dev)._

It'll build Exo (may take a few minutes), then ask for the API key and your name
and your agent's name, and give you the command to start Exo (./exo.sh).

<div align="center">
<img height="300" alt="exo-img" src="https://github.com/user-attachments/assets/b8fafe1c-7814-4681-b6f6-bed6859c7835" />
</div>

## Basic Agent Interaction

For the basic setup of Exo, there are two methods of interacting: on the command
line where you ran the setup script (or `exo.sh`), and through a browser using
exo-chat.

Exo agents are intended to be long-running. For example, if you `/exit` from the
command line you can still interact with it via exo-chat. And if you do exit,
you can always connect back to the CLI chat using `./exo.sh`.

Exo-chat is a minimal, web-based chat where you can talk to your agent from
anywhere on the internet. However, Exo also supports standard chat applications
like IRC, Discord, WhatsApp, Signal, or Slack. To configure them, just ask your
agent to do so.

If you ever forget or lose your exo-chat URL, you can just ask Exo for it from
the command line.

A good end-to-end test is to have it install a tool in the sandbox and use it with
the task scheduler. For example, try the following prompt:

```
Install python3 and curl in the sandbox. You don't need sudo, just use apt-get. Once you've done that, please
schedule a task to run every minute that grabs news headlines from the BBC RSS feed. Only print new headlines you've
not printed before. Please print them here.
```

## Operating Exo

`setup.sh` is only for the first-time install. After that, `./exo.sh` in the
repo root is the day-to-day control surface for your agent. Running it with no
arguments starts (or reconnects to) the local agent so you can talk to it via
the command line.

```
./exo.sh                # start the full stack (Docker sandbox, ExoChat) and open the CLI chat interface
./exo.sh list           # list agents and conversations
./exo.sh stop-all       # stop the scheduler and adapter runners; state is preserved
./exo.sh fresh          # rebuild, delete all agents/conversations, start clean
./exo.sh setup-profile  # update your local profile (name, preferences)
./exo.sh --help         # all commands and options
```

The most common command for the `./exo.sh` starter script: use `stop-all` when you want to shut Exo down,
plain `./exo.sh` to bring it back with all state intact, and `fresh` when you
want to throw everything away and start over with a brand-new agent.

By default `./exo.sh` uses the `canonical` template: a Docker sandbox, the repo
mounted at `/workspace/exo`, and ExoChat for remote access. Pass
`--template dev` for a developer variant that sets up IRC and Discord instead
of ExoChat, or `--template minimal` for a bare REPL with no Docker defaults or
adapter setup.

## Understanding Exo

There are only a few key components you need to know about to understand how
Exo works. You can use Exo like any agent without understanding these internals.
But having a basic idea will help you more effectively guide Exo if you want it
to evolve itself. For a deeper dive into these concepts, see
[`docs/EXO-BASICS.md`](docs/EXO-BASICS.md).

**Basic Loop** Exo runs a host-side loop that receives user messages and adapter
events, builds the model context, exposes the active tools, executes tool calls,
and records the results. This loop runs outside the sandbox.

**Sandbox** Exo uses a vanilla unbuntu sandbox where
it can install packages, run commands, and experiment. It can snapshot and
rewind that sandbox when it needs to back out from changes.

**Tools and Adapters** These are the core methods for Exo to interact with the
world and with itself. Tools are functions the model can call, such as executing
a shell command with the `shell` tool. Adapters are long-running host processes
for stateful external channels such as ExoChat, IRC, WhatsApp, Signal, and
Discord.

**Canonical State** Exo stores durable conversation history, tool activity,
adapter events, artifacts, and sandbox records outside the sandbox filesystem.
This state is not rewound when the sandbox is rewound, so the agent can
reconstruct what happened across experiments, restarts, and rebuilds.

**Source Code** Exo's source code is mounted in the
sandbox at `/workspace/exo`. The agent can read and modify that code and has
tools that allow it to rebuild and restart itself and all components. This allows Exo to be able to modify every aspect of itself.

**REPL and ExoChat** The minimal setup gives you two ways to talk to Exo: the
local REPL, which is a command-line chat interface, and ExoChat, a simple
text-only web chat hosted at `https://exoharness.ai`.

## Where to Go From Here

If you have your agent up and running, there really is little else you need to understand or do other than talk to it and ask it to evolve itself in a direction that you want. It already contains basic support for services such as WhatsApp, Slack, Discord, Signal, and IRC. But it's very easy to extend it to support more things

## Shortcomings

While there are many, the most obvious is that right now there isn't a simple way for Exo to do generalized computer use of a windowed system. This is in the works and should land soon. But in the meantime, you can get a long way by asking Exo to build such a thing for itself.

## Tweaking Prompts

There are a number of prompt files that Exo uses during runtime. You can edit these directly or ask Exo to.

- `examples/exo/prompts/me.md`: the committed core identity and operating
  rules for the default Exo agent.
- `.exo/exo-profile.md`: local, git-ignored profile instructions such as
  your name and machine-specific preferences. Create or update it with
  `./exo.sh setup-profile`.
- `examples/exo/harness.ts`: assembles the full prompt sent each turn,
  including dynamic instructions about tools, adapters, memory, sandbox behavior,
  and self-maintenance.

After changing prompt files, ask Exo to rebuild/restart itself for them to go in
use.

## Ongoing Work

Exo is still in the early stages of development, with many areas of active
work. We welcome participation ranging from core development to feedback from
users. Current areas of focus include:

- **Autonomous self-maintenance:** Exo should periodically inspect its context,
  memories, tools, scheduled tasks, and running processes; identify stale or
  conflicting state; propose or perform safe cleanup; and record what changed.
  Success means a long-running agent becomes more organized over time rather
  than accumulating context and tool rot.
- **Recoverable, portable execution:** Schedulers, adapters, and other
  long-running work should persist enough state to resume after process,
  machine, or network failure. Work should be movable between compatible
  machines without losing canonical history or duplicating side effects.
- **High-level multi-agent orchestration:** Cloning and lineage are useful
  primitives, but agents also need policies for when to clone, how to divide
  work, how children report results, how conflicting conclusions are resolved,
  and when a lineage should stop. The goal is a family of agents that produces
  better results than one agent without creating unbounded cost or coordination
  overhead.

## License

MIT
