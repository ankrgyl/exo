# Tools and Adapters

This document describes how tools and adapters work today, functionally rather
than file by file.

## The Boundary

A **tool** is a bounded operation the model requests during an active turn. The
harness executes it and returns a result to that same turn.

An **adapter** is a supervised bridge to an external system. It runs between
turns, receives external events, and can wake a conversation.

In short:

- Tools let the agent call out during a turn.
- Adapters let external systems call in between turns.

Tool requests and results are durable conversation events; large results are
stored as artifacts. Adapter identity, routing, message state, and health live
outside conversation context and survive compaction.

## Tools

### The registry

Installed tools live in one workspace-local registry. A tool is installed from
one of two source types:

- a **workspace-relative directory** (copied into a managed store)
- a **Git repository pinned to an exact commit**, optionally selecting a
  subdirectory

Each source carries a minimal manifest with a schema version, a stable id, and
a module entrypoint. Everything else — the model-facing name, description,
argument and output schemas, and the handler — is owned by the TypeScript
module itself. Installing a source whose stable id is already installed
replaces the existing installation; there is no separate upgrade action.

Installation validates before it commits: the source is staged, the manifest is
parsed, the module is loaded and initialized, the argument schema is checked
against the model API's strict-mode rules (every property listed in `required`,
optional parameters expressed as nullable types, `additionalProperties: false`
at every object level), and the tool name is checked for collisions. Only then
does the registry record change. Failed installs leave no partial state. The
same strict-schema check runs again when installed tools are registered at the
start of each round, so a bad tool is skipped with a logged error instead of
poisoning the model call for every turn. A successfully installed tool becomes
callable on the next model round.

### Management and inspection

- `manage_tool` is the only write surface: `install` and `remove`.
- `inspect_tools` is read-only: `list` and `get` over either the tools active
  in the current round or the tools installed in the registry.
- The operator CLI is read-only: `exo tools list` and `exo tools get <id>`.

Local source paths are resolved on the host relative to the workspace root.
Because the agent's shell runs in a sandbox where the workspace is mounted at
`/workspace/exo`, an agent authors a tool under
`/workspace/exo/.exo/tool-sources/<name>` and installs it with the relative
path `.exo/tool-sources/<name>`. Absolute sandbox paths such as `/tmp/...` are
rejected because they do not exist on the host.

The legacy direct-source creation tools (`install_agent_tool`,
`uninstall_agent_tool`) are an opt-in compatibility path and default off.

### Execution and trust

Tool modules run as trusted TypeScript code in the harness process. Loading a
tool is a trust decision, not a security boundary. Tools may use harness APIs
for sandbox processes, artifacts, and secrets; secret values must never appear
in model-visible definitions, prompts, or results. Credentials reach tools
through environment variables or secret references, not raw values in
configuration: an initialization value of exactly `${ENV_VAR}` is resolved from
the host environment each time the tool loads, so the raw value never enters
the tool lockfile.

Deferred, deliberately: capability sandboxes, remote registries, signatures,
publisher trust, and generated command tools.

### Profiles

Profiles are curated tool sets, not trust levels:

- **bootstrap** — `shell`, `inspect_tools`, `manage_tool`, and
  `rebuild_and_restart_exo`: enough to inspect, extend, and repair the system.
- **practical** (default) — bootstrap plus scheduler, adapter, sandbox
  recovery, introspection, memory, todo, skill, and web tools.

Select a profile with `./exo.sh --profile bootstrap|practical` or the
`EXO_PROFILE` environment variable.

## Adapters

Adapters are supervised worker processes with a lifecycle independent of model
turns: starting, running, disabled, or error. The host owns supervision,
routing, durable message records, retries, and conversation wakeups.
Protocol-specific behavior stays in each worker.

The shipped adapter library includes IRC, ExoChat, WhatsApp, Signal, Discord,
Slack, and agent-cli. `create_adapter` instantiates one of these library types
for a conversation with a typed configuration; adapters are then managed with
`list_adapters`, `enable_adapter`, `disable_adapter`, and `delete_adapter`.

Inbound events are authenticated, deduplicated, persisted, and routed to the
owning conversation as wakeups. Outbound messages are explicit
`send_adapter_message` calls with durable delivery states: a message is queued,
claimed in-flight by the worker, and then delivered or failed. A rejected
delivery is retried up to three attempts before it is recorded as terminally
failed. Delivered and failed records are retained for inspection.

Credentials are secret references (for example `passwordSecretId`) resolved by
the host; raw secret values never appear in adapter configuration.

Scheduling remains a dedicated service with its own tools; it is not an
adapter.

## Example: The Agent Builds Its Own Tool

This exercises the full self-extension loop: author a source in the sandbox,
install it through the registry, and call it.

### Prompt

```text
Build and install a managed tool named `word_stats`.

1. Use shell to create the source directory
   /workspace/exo/.exo/tool-sources/word-stats containing:
   - exo-tool.json with schemaVersion 1, id "tool:local/word-stats",
     and module "tool.ts"
   - tool.ts default-exporting a Tool (type-only imports from
     @exo/harness/tool) that takes a required `text` string and returns
     word count, character count, and the five most frequent words.
   Use strict JSON schemas: additionalProperties false, every property listed
   in required.

2. Install it with manage_tool:
   action "install", source {type: "local", path: ".exo/tool-sources/word-stats",
   subdirectory: null}, initialization null, toolId null.

3. Report the installed tool id and stop. The tool becomes callable on my
   next message.
```

Then, in the next message:

```text
Use word_stats to analyze the following paragraph: <paragraph>
```

### Inspecting and debugging

- Ask the agent to call `inspect_tools` with source `installed` to confirm the
  registry entry, or source `active` to confirm the tool is registered this
  round.
- From a terminal, `exo tools list` and `exo tools get tool:local/word-stats`
  show the same installation state read-only.
- Install failures come back directly in the `manage_tool` result: schema
  violations, name collisions, path problems, and module load errors are
  reported with the reason. Nothing is partially installed on failure.
- A tool that installs but later breaks is logged and skipped at load time
  rather than blocking startup; reinstalling the same id replaces it.
- If the agent claims it cannot see a freshly installed tool, remember the
  one-round delay: installation succeeds in the current round, and the tool is
  registered at the start of the next one.

## Example: The Agent Builds Its Own Adapter

New adapter types are part of Exo itself, so this goes through the
self-modification path: edit the mounted source tree, rebuild and restart, then
instantiate. This is heavier than building a tool — use an adapter only when
something external must reach the agent between turns.

### Prompt

```text
Add a new adapter type called `webhook` to your own source tree and use it.

1. Read your self-map first, then study the IRC adapter as the reference
   implementation for the worker protocol, inbound event delivery, and
   acknowledgements.

2. Implement a webhook adapter: a worker that listens on a configurable local
   port, accepts POSTed JSON, and delivers each request as an inbound event
   that wakes this conversation. Wire up its creation config alongside the
   existing adapter types.

3. Run rebuild_and_restart_exo to validate, build, and activate the change.

4. After the restart, create a webhook adapter for this conversation on
   port 8787 and tell me the adapter id.
```

Then verify end to end, for example by POSTing JSON to the port and asking the
agent what it received, or in a follow-up message:

```text
Check list_adapter_events for the webhook adapter and summarize the most
recent inbound events.
```

### Inspecting and debugging

- `list_adapters` shows each adapter's enabled state and health fields:
  `last_connected_at_ms` and `last_error`. Pass `includeDisabled: true` to see
  disabled adapters.
- `list_adapter_events` shows the durable event history — connected,
  disconnected, inbound, outbound, and error events — filterable by type and
  time.
- `list_conversation_events` shows what actually woke the conversation, which
  separates "the adapter received it" from "the conversation was woken".
- Outbound problems are visible as delivery states: a message stuck in queued
  means the worker is not claiming work; repeated failures become a terminal
  failed record after three attempts, with the error retained.
- `disable_adapter` and `enable_adapter` restart a misbehaving worker without
  losing its history; `delete_adapter` removes it and its history entirely.
- If the rebuild fails, `rebuild_and_restart_exo` records the failure durably
  and the previous binary keeps running; fix the code and queue it again.

## Design Rules

- Keep model-facing tool definitions small and provider-neutral.
- Validate arguments before execution and record structured failures.
- Keep tools bounded to the active turn; anything that listens is an adapter.
- Keep adapter state durable and independent of conversation compaction.
- Treat locally loaded modules as trusted code.
- Prefer explicit local state over speculative registry or policy machinery.
