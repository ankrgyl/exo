---
title: Your First Session
description: Talk to your agent, give it a real task, and teach it who you are.
---

# Your First Session

After [setup](./installation) you have two ways to talk to your agent:
the **local REPL** in your terminal, and **ExoChat** — the
`https://exoharness.ai/chat?...` URL the setup printed, which works from any
browser.

## A good end-to-end test

Exo's canonical agent can install tools in its sandbox and schedule
recurring work. This prompt exercises both:

```text
Install python3 and curl in the sandbox. You don't need sudo, just use
apt-get. Once you've done that, please schedule a task to run every minute
that grabs news headlines from the BBC RSS feed. Only print new headlines
you've not printed before. Please print them here.
```

If that works, you have a fully functional agent: sandbox package installs,
the task scheduler, and conversation wakeups are all live.

## Things to try

- **Snapshot before an experiment.** Ask it to snapshot its sandbox, try
  something risky, then rewind if it goes badly.
- **Ask it to build a tool.** The agent can write and install its own tools
  at runtime; they persist under `.exo/agent-tools/`.
- **Add a channel.** It ships with adapter support for WhatsApp, Signal,
  Discord, and IRC — ask it to set one up.
- **Ask it to evolve.** Its own source tree is mounted in the sandbox at
  `/workspace/exo`. It can read its harness, modify it, rebuild, and restart
  itself.

## Tweaking prompts

Exo reads several prompt files at runtime. Edit them directly, or ask the
agent to:

- `exo/prompts/me.md` — the committed core identity and
  operating rules of the default agent.
- `.exo/exo-profile.md` — your local, git-ignored profile (name,
  machine-specific preferences). Recreate it anytime with
  `./exo.sh setup-profile`.
- `exo/harness.ts` — assembles the full prompt each turn,
  including dynamic instructions about tools, adapters, memory, and
  self-maintenance.

After changing prompt files, ask the agent to rebuild and restart itself for
them to take effect.

## Managing the agent

`./exo.sh` is the control surface for the canonical agent:

```bash
./exo.sh              # create or reuse the agent, start services + REPL
./exo.sh list         # list agents and conversations
./exo.sh stop-all     # stop scheduler and adapter loops
./exo.sh fresh        # start over with a fresh agent
./exo.sh setup-profile
```

Where to go from here: there's little else you *need* to learn — talk to the
agent and ask it to evolve in the direction you want. When you're curious
how it all works, read [Concepts](../concepts/index); when you want to
build your own agent instead of using the canonical one, start with
[Custom Agent Quickstart](../tutorials/write-your-own-agent).
