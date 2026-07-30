# Exo Basics

## Key Concepts

### Exo source code

In the canonical Exo setup, the running source tree is mounted into the
agent sandbox at `/workspace/exo`. This lets the agent inspect its own harness,
prompts, tools, adapters, scheduler, and startup scripts, and propose or make
changes to them.

### Canonical state

Exo stores conversation history, tool activity, host lifecycle events, adapter
events, artifacts, and sandbox records outside the sandbox filesystem. That
durable history is not rewound when the sandbox is rewound, so the agent can
reconstruct what happened across restarts, rebuilds, and experiments.

### Sandbox

Canonical Exo conversations use a shared agent-scoped sandbox by default.
The agent can run shell commands there, install tools, inspect snapshots, create
new snapshots, and rewind the sandbox when it needs to back out risky changes.

### Guardian

The guardian is a host-side control surface for maintenance that should happen
outside the sandbox. The agent calls `rebuild_and_restart_exo` for the fixed
self-update path. Operators use the guardian CLI to inspect service status and
logs or perform targeted service control while preserving `.exo` state.

### Tools

Tools are functions the model can call during a turn. The bootstrap surface is
`shell`, `inspect_tools`, `manage_tool`, and `rebuild_and_restart_exo`; the
practical profile adds tools for adapters, scheduling, sandbox snapshots,
memory, and introspection. Legacy agent-tool creation is opt-in.
Tool definitions are registered each model round, so the agent sees the current
tool list as part of the model request.

### Adapters

Adapters are long-running host processes that connect an agent conversation to
external surfaces such as ExoChat, IRC, WhatsApp, Signal, Discord, or a local
shell CLI. They own protocol sockets, reconnect behavior, inbound event history,
conversation wakeups, and outbound sends. The canonical setup starts ExoChat by
default and prints a browser URL for it.

### Scheduler

Exo also includes a task scheduling process that manages recurring sandbox work
(for example, once an hour). The agent can create, list, cancel, and delete
scheduled tasks, and each completed run can wake the conversation with a compact
result. A task can also be a one-shot (`@at <rfc3339>`), which fires once and is
then marked completed while staying visible in listings. Recurring fires land on
the schedule's own grid rather than on when the last run finished, and every task
carries a missed-fire policy saying what a host that was down owes it: drop the
missed slots, fire one catch-up, or fire all of them.

### Memory

Exo includes `remember` and `forget` tools for durable agent memory. Saved
memory is stored outside the sandbox and injected back into future turns across
conversations.

## Tools

Exo has the following minimal set of tools to control and interact with its environment and to evolve itself.

### Core

- Host control: `shell`
- Tool discovery and management: `inspect_tools`, `manage_tool`
- Self-maintenance: `rebuild_and_restart_exo`
- Legacy compatibility, when explicitly enabled: `install_agent_tool`,
  `uninstall_agent_tool`

### Agent

- Adapter tools: `create_adapter`, `list_adapters`, `enable_adapter`,
  `disable_adapter`, `delete_adapter`, `send_adapter_message`.
- Adapter/event introspection: `list_adapter_events`,
  `list_conversation_events`.
- Scheduler tools: `schedule_sandbox_task`, `list_scheduled_tasks`,
  `cancel_scheduled_task`, `delete_scheduled_task`.
- Sandbox tools: `list_sandbox_snapshots`, `snapshot_sandbox`,
  `rewind_sandbox`, `get_sandbox_status`.
- Memory: `remember`, `forget`.

## Adapters

Supported adapters:

- `exochat`: a hosted, text-only browser chat at `https://exoharness.ai`.
- `irc`: an IRC channel adapter for lightweight text chat.
- `whatsapp`: a WhatsApp linked-device adapter using Baileys.
- `signal`: a Signal linked-device adapter using `signal-cli`.
- `discord`: a Discord bot adapter with message and attachment support.
- `slack`: a Slack bot adapter with channel, thread, and DM support.
- `agent-cli`: a local shell adapter for sending prompts from any directory.
