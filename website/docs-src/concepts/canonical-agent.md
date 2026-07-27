---
title: The Canonical Agent
description: What setup.sh actually runs — the canonical exo agent, its tools, and its self-improvement loop.
---

# The Canonical Agent

The [setup script](../getting-started/installation) launches exo's
**canonical agent** — a long-running personal agent (its implementation
lives at `examples/exo/`) that demonstrates the full
recursive-self-improvement loop. You can use it without knowing these
internals, but a basic map helps when you want to guide its evolution.
For how each piece starts, stays up, and stops, see [Lifecycles](./lifecycles).

## The pieces

**Basic loop.** A host-side loop receives user messages and adapter events,
builds the model context, exposes the active tools, executes tool calls, and
records the results. This loop runs *outside* the sandbox.

**Sandbox.** A vanilla Ubuntu container where the agent installs packages,
runs commands, and experiments. It can snapshot the sandbox and rewind it to
back out changes.

**Canonical state.** Conversation history, tool activity, adapter events,
artifacts, and sandbox records are stored *outside* the sandbox filesystem.
This state is not rewound when the sandbox is rewound, so the agent can
reconstruct what happened across experiments, restarts, and rebuilds.

**Source code.** The agent's own source tree is mounted in the sandbox at
`/workspace/exo`. It can read and modify that code, and it has tools to
rebuild and restart itself and its services. This is what makes *every*
aspect of the agent modifiable.

**Guardian.** A host-side control surface for maintenance that must happen
outside the sandbox. Through `guardian_action` the agent can build Exo,
inspect service status and logs, and restart the scheduler or adapter
runners — all while preserving `.exo` state.

**Scheduler.** A task-scheduling process for recurring sandbox work (e.g.
hourly). The agent can create, list, cancel, and delete scheduled tasks;
each completed run can wake the conversation with a compact result.

**Memory.** `remember` and `forget` tools give the agent durable memory,
stored outside the sandbox and injected back into future turns across all
conversations.

## The tool surface

Core:

- Host control: `shell`
- Tool management: `install_agent_tool`, `uninstall_agent_tool`

Agent:

- Adapters: `create_adapter`, `list_adapters`, `disable_adapter`,
  `delete_adapter`, `send_adapter_message`
- Introspection: `list_adapter_events`, `list_conversation_events`
- Scheduler: `schedule_sandbox_task`, `list_scheduled_tasks`,
  `cancel_scheduled_task`, `delete_scheduled_task`
- Sandbox: `list_sandbox_snapshots`, `snapshot_sandbox`, `rewind_sandbox`
- Self-maintenance: `guardian_action`
- Memory: `remember`, `forget`

Tool definitions are registered fresh each model round, so tools the agent
installs mid-turn are visible in the very next round.

## Why the layering matters here

The canonical agent is the payoff of the
[exoharness/executor split](./exoharness-and-executor): everything above
the substrate — prompts, tools, harness code, even the scheduler — is fair
game for the agent to modify, because the event log, secrets, and snapshots
below it guarantee a way back.
