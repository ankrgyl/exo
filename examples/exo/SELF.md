# Exo Self Map

This file helps Exo inspect and maintain its own code. In a normal local
startup, the repository is mounted in the sandbox at:

```text
/workspace/exo
```

Use this map before changing Exo itself.

## Important Paths

- `examples/exo/harness.ts`: assembles Exo's prompt and tool registry.
- `examples/exo/prompts/me.md`: durable identity and operating rules.
- `examples/exo/guardian-tools.ts`: model-visible host maintenance tool.
- `examples/exo/scripts/exo-service-guardian`: host-side build and service control.
- `./exo.sh`: local startup script for REPL, scheduler, adapters, sandbox, and repo mount.
- `examples/exo/sandbox-tools.ts`: sandbox snapshot and rewind tool definitions.
- `examples/exo/introspection-tools.ts`: `list_adapter_events` and `list_conversation_events` introspection tools.
- `examples/exo/host-tools.ts`: `registerHostTool` helper that bridges TypeScript tool definitions to Rust execution.
- `examples/exo/scheduler-tools.ts`: scheduled task tool definitions.
- `examples/exo/scheduler-runner/`: host scheduler runner binary.
- `examples/exo/adapters/`: adapter setup prompts and worker implementations.
- `examples/exo/adapters/agent-cli/`: shell entry point adapter; `exo-cli` sends prompts plus the user's working directory over a unix socket, and the message tells you which sandbox path (under the `/agent-cli` mount by default) to `cd` into.
- `typescript/harness/adapter-tools.ts`: model-visible adapter tool definitions.
- `crates/executor/src/adapter/`: Rust adapter runtime and supervision.
- `crates/executor/src/harness_tool.rs`: Rust tool execution runtime (`execute_tool` match arms).
- `crates/executor/src/agent_sandbox.rs`: shared agent sandbox selection.
- `crates/executor/src/conversation_sandbox.rs`: conversation sandbox selection.
- `crates/exoharness/`: durable harness API, conversation state, events, artifacts, and sandbox lifecycle.

## Local State

- `.exo/` contains local harness state, adapter config, pairing data, artifacts,
  pid files, logs, and service guardian config. It is intentionally ignored by
  git.
- `.env` contains local secrets and environment configuration. It is ignored by
  git.
- Do not commit personal profile data from `.exo/exo-profile.md`.

## Common Commands

From the repository root on the host:

```bash
./exo.sh
examples/exo/scripts/exo-service-guardian status
examples/exo/scripts/exo-service-guardian build
examples/exo/scripts/exo-service-guardian restart-all --build
./exo.sh --control
```

Inside the Exo sandbox, inspect the mounted code with:

```bash
cd /workspace/exo
pwd
ls examples/exo
```

## Tool Architecture

Tools have two layers, and both matter:

1. **Definition (TypeScript)**: the model only sees tools registered in the
   TypeScript registry each turn (`examples/exo/harness.ts`). Sources are
   built-in tools, library tool modules, and registry-installed tools managed
   with `inspect_tools` / `manage_tool`. The older `.exo/agent-tools/` scan and
   `install_agent_tool` / `uninstall_agent_tool` tools are compatibility
   surfaces enabled only by `enableAgentToolCreation`.
2. **Execution (TypeScript or Rust)**: a tool's handler can run entirely in
   TypeScript, or delegate to the Rust runtime via
   `execution.context.executeTool(...)`, which dispatches on the function name
   in `crates/executor/src/harness_tool.rs`.

For a local manifest tool, create its directory under
`/workspace/exo/.exo/tool-sources/<name>` and call `manage_tool` with the
workspace-relative path `.exo/tool-sources/<name>`. `shell` sees sandbox paths,
while `manage_tool` resolves local sources in the host harness; never pass
`/tmp/...` or another absolute sandbox path.

### Adding a Rust-backed tool

A Rust match arm alone is invisible to the model; it always needs a TypeScript
definition that delegates to it:

1. Implement the tool logic as a match arm in `execute_tool` in
   `crates/executor/src/harness_tool.rs` (see `list_sandbox_snapshots` for a
   full example).
2. Register a TypeScript definition with the same name using
   `registerHostTool` from `examples/exo/host-tools.ts`, wired into the
   registry in `examples/exo/harness.ts` (see `sandbox-tools.ts` for the
   pattern).
3. Call `rebuild_and_restart_exo` so both the Rust binary and the harness pick
   up the change.
4. Never create an agent-installed tool with the same name as a Rust-backed
   tool; the registry conflict makes calls ambiguous.

## Diagnosing Adapters and Restarts

When an adapter seems quiet, broken, or recently restarted, diagnose it from
inside the conversation before touching host services:

1. `list_adapters` shows each adapter's `enabled` state plus health fields
   `last_connected_at_ms` and `last_error`.
2. `list_adapter_events` returns the adapter's recent telemetry newest first:
   `connected`, `disconnected`, `inbound`, `outbound`, `error`, and
   `lifecycle` records. Filter with `eventType` and `sinceMs` to narrow in
   (for example `eventType: "error"` after a reboot, or `sinceMs` set to the
   restart time).
3. `list_conversation_events` reads the canonical conversation event log,
   which host components also write to. `host_reboot` records a planned host
   restart with its reason; `adapter_runner_started` records every adapter
   runner start (without a preceding `host_reboot` it implies a crash or
   manual restart); `adapter_runner_draining` records a graceful shutdown
   beginning. This is the immutable history of what happened to you — use it
   to answer "was I restarted, when, and why?".
4. Only ask the operator to inspect guardian logs once the event history shows
   a host-side problem (worker crash loops, repeated disconnects, send failures).

## Maintenance Rules

- Use `rebuild_and_restart_exo` for the fixed self-update build-and-restart path.
- Service status and logs are operator CLI responsibilities:
  `examples/exo/scripts/exo-service-guardian status` and
  `examples/exo/scripts/exo-service-guardian logs`.
- Use the `shell` tool for sandbox-local inspection and experiments.
- Use `snapshot_sandbox` before risky filesystem changes.
- Use `rebuild_and_restart_exo` after code changes that require host services
  to pick up a new build. The restart is deferred briefly so the current turn
  can finish before services stop.
- In control mode, service guardian builds write `.exo/exo-control.restart`;
  the `./exo.sh --control` wrapper restarts only the child `exo repl` and
  keeps the user's terminal open.
- Service restarts drain gracefully: the guardian writes
  `.exo/exo-adapters.restart` / `.exo/exo-scheduler.restart`, the
  runner claims the marker, finishes in-flight work, and exits so the guardian
  can start the new build. Runners that do not claim the marker are killed
  after a short wait.
- Adapter restarts also write `.exo/exo-reboot-notice.json`; the fresh
  adapter runner claims it and wakes the adapter conversations so you can
  announce externally that you are back. Announce planned downtime with
  `send_adapter_message` before requesting the restart.
- Preserve `.exo` state unless the user explicitly asks to delete state.
