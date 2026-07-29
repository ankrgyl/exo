# Exo Self-Control

Exo is meant to be a long-running agent that can understand, maintain, and evolve the system it is operating inside.

This document defines the self-introspection and self-control capability areas we want Exo to have, and for each one, what exists in the codebase today and what is still missing. The capability areas are:

1. **Self-knowledge**: see all of its own code, edit it, build it, and rerun itself on the result.
2. **Durable memory and identity**: know what state constitutes "itself", which parts survive restarts, rewinds, and rebuilds, and keep an append-only event log so it knows what it has tried even after rolling state back.
3. **Component observability**: logs and telemetry for the scheduler, adapters, tools, and host services, to debug and evolve them.
4. **Sandbox control**: snapshot, rewind, and (eventually) clone its execution environment.
5. **Capability extension**: add tools and adapters for itself.
6. **Prompt evolution**: inspect and change the prompts that define its own behavior.
7. **Cloning and migration**: reproduce itself on another machine or platform. _(Not yet built.)_
8. **Self-verification and rollback**: validate a change to itself before adopting it, and undo it cleanly when it fails.

## Mutation and Transparency Design Principle

- Every self-modification should be auditable after the fact: from the canonical
  event log, git history, and host logs it should be possible to reconstruct what
  Exo changed about itself and why.
- Every aspect of the system should be visible to the agent, from code to logs to events.
- Durable mutations go through named tools with clear schemas, not hidden
  conventions or ad-hoc file edits to host state. Explicit surfaces are what make
  the audit trail complete: a mutation path that bypasses the tools also bypasses
  the record.
- Mutations are reversible by default: prefer disabling and checkpointing over
  deletion and irreversible rewrites unless the user asks for a permanent change.
  An audit record of an unrecoverable action is not a substitute for being able
  to undo it.

## 1. Self-Knowledge: Code, Build, and Rerun

Exo should always be able to inspect the code that defines its own behavior, change it, and restart itself on the new build.

**Seeing its own code.** Local startup with `./exo.sh` mounts the repository into the sandbox at `/workspace/exo` (configurable with `--self-repo-mount` or `EXO_REPO`). The shell tool therefore starts with a stable view of the source tree. `exo/SELF.md` is the checked-in self map — a compact navigation guide to prompts, harness assembly, adapters, scheduler, supervisors, and local state — and the harness tells Exo its path each turn through `EXO_SELF_MAP`. The self map is intentionally navigational, not a full architecture document.

**Editing its own code.** Edits happen through the shell tool against the repo mount. Git is the change history and the rollback mechanism for code: Exo can diff, commit, and revert its own modifications (see area 8).

**Building and rerunning itself.** Host-side actions happen outside the sandbox through two cooperating supervisors:

- `exo/scripts/exo-service-guardian` is the host service supervisor: it builds Exo, shows service status, prints scheduler and adapter logs, and restarts the scheduler or adapter runners while preserving `.exo` state.
- `./exo.sh --control` is the terminal supervisor: it keeps the user's terminal open, streams service logs, runs the interactive `exo repl` as a child, and can restart only that child after a rebuild.

The model-visible `rebuild_and_restart_exo` tool exposes one fixed self-update operation: validate and build Exo, then hand a scheduler and adapter restart to a detached guardian process so the current model turn can finish. Pass `reason` as a short free-text note describing the change being activated. On completion, the outcome is written under `.exo/guardian-updates/` and also appended to this conversation's event log as a `rebuild_and_restart_exo` host event (succeeded or failed), so `list_conversation_events` can answer whether a rebuild finished. Service status, logs, and targeted service controls stay in the operator-facing `exo/scripts/exo-service-guardian` CLI.

Service restarts drain instead of killing blindly: the guardian writes a restart marker (`.exo/exo-adapters.restart` or `.exo/exo-scheduler.restart`); the runner claims the marker, finishes in-flight wakeup turns or scheduler passes, and exits on its own. A runner that never claims the marker is stopped with a process-tree kill after a short wait. Adapter workers run in their own process groups so stopping a worker also terminates the `pnpm`/`tsx`/`node` children holding the external connection. Builds also write `.exo/exo-control.restart`, which the control wrapper claims to restart only the `exo repl` child without closing the user's terminal.

Reboots are announced through the adapters: because restarts are deferred, the agent can post a "going down" message with `send_adapter_message` in the same turn that requests the restart. The guardian writes `.exo/exo-reboot-notice.json`; the fresh adapter runner claims it and sends one wakeup per adapter conversation so the agent can announce its return. Announcements queue in the durable adapter outbox and deliver when the worker reconnects. Stale notices (older than 15 minutes) are discarded.

The canonical local startup is plain `./exo.sh` (Docker provider/backend, repo self-map mount, guardian config, adapter setup prompts, control REPL — the default `--template canonical`). `./exo.sh fresh` gives the same shape from a clean agent/conversation state.

## 2. Durable Memory and Identity

Exo's identity is its durable state. Rollback, cloning, and migration all depend on knowing which state is identity-critical and what survives each kind of reset:

| State                                       | Lives in                  | Survives sandbox rewind    | Survives service restart       | Checked in |
| ------------------------------------------- | ------------------------- | -------------------------- | ------------------------------ | ---------- |
| Code + prompts                              | git repo                  | yes                        | yes                            | yes        |
| Conversation history + event log            | `.exo/exoharness`         | yes                        | yes                            | no         |
| Agent artifacts (sandbox registry, configs) | `.exo/exoharness`         | yes                        | yes                            | no         |
| Adapter + scheduler records                 | `.exo`                    | yes                        | yes                            | no         |
| Secrets                                     | Keychain / secret backend | yes                        | yes                            | no         |
| Local profile memory                        | `.exo/exo-profile.md`     | yes                        | yes                            | no         |
| Sandbox filesystem                          | sandbox backend           | **no** (that is the point) | yes (warm sandbox)             | no         |
| Worker connections                          | adapter worker processes  | yes                        | **no** (reconnect after drain) | no         |

The operating rules that follow: preserve `.exo` unless the user explicitly asks to delete state; never store durable memory only in the sandbox filesystem (it is the one resettable layer); put user-specific memory in the local profile and behavioral changes in checked-in prompts; and treat secrets as the one category that cannot be casually copied or recreated.

**The immutable event log.** Within this inventory, the exoharness conversation event log is the append-only record that lets Exo answer "what happened to me, and what have I already tried?" — especially after rolling back sandbox state, when the filesystem no longer reflects past attempts. It captures sessions, turns, messages, tool requests and results, errors, artifact writes, and sandbox lifecycle events. Host components write into the same log as `Custom` events so host actions are part of the immutable history rather than living only in side channels: the adapter runner appends `host_reboot` when it claims a reboot notice (with the guardian's reason), `adapter_runner_started` on every start (a start without a preceding `host_reboot` implies a crash or manual restart), `adapter_runner_draining` when a graceful drain begins, and `rebuild_and_restart_exo` when a deferred self-update finishes (succeeded or failed, with `updateId`, `exitCode`, and the tool `reason`).

The agent reads this history back with `list_conversation_events`, which defaults to lifecycle and host event kinds (sessions, errors, sandbox events, host events) and supports explicit kind filters, pagination cursors, and ordering — including the per-turn traffic kinds (`messages`, `tool_requested`, `tool_result`) when the agent needs to reconstruct exactly what a past turn did.

Two properties matter for rollback:

- **Rewind does not erase history.** `rewind_sandbox` restores sandbox filesystem state; it does not roll back conversation events, artifacts, adapter records, scheduler records, or secrets. `snapshot_sandbox` records a `sandbox_snapshotted` event and `rewind_sandbox` records a snapshot-backed `sandbox_started` event in the same active turn, so later readers can reconstruct what was rolled back and when.
- **Git is the second immutable log.** Code changes are visible in git history even after a sandbox rewind, because the repo mount is host-backed.

Gap: host events currently cover the adapter runner only. The scheduler runner and the control REPL do not yet write start/drain/crash events into the log, and there is no agent-level (cross-conversation) event stream — host events are fanned out to each adapter-attached conversation.

### Agent-writable memory (the `remember` tool)

Everything above is memory the agent reads; until recently it had no way to write a durable fact about the user or itself. Conversation history is replayed in full each turn, but it dies at the conversation boundary, and the local profile (`.exo/exo-profile.md`) is human-curated.

The memory tool closes that gap with a small artifact-backed store:

- **Storage.** One JSON artifact at `memory/exo-memory.json` on the agent handle, so it persists across every conversation for this agent.
- **Write path.** `remember(text)` appends `{ id, text, createdAt }`; `forget(id)` removes one entry. A fixed cap drops the oldest entries if the store grows too large.
- **Read path.** Prompt assembly reads the latest memory artifact and adds a developer message listing saved facts with ids, so the model can use them and delete stale ones.

This is deliberately not embedding-based retrieval. For a small set of short facts, always injecting the whole store is simpler and easier to audit. If the memory set grows beyond what should fit in every prompt, the storage layer can stay the same while the read path evolves toward query-time recall.

## 3. Component Observability: Logs and Telemetry

Exo should be able to inspect each of its components when debugging or evolving them, before escalating to restarts.

- **Host services**: operators use `exo/scripts/exo-service-guardian status` for service state and `exo/scripts/exo-service-guardian logs` for scheduler and adapter runner logs. The agent uses `rebuild_and_restart_exo` only for its fixed self-update flow. Deferred rebuild output is written to `.exo/exo-service-guardian-actions.log`.
- **Adapters**: `list_adapters` returns each adapter's `enabled` state plus health fields `last_connected_at_ms` and `last_error`. `list_adapter_events` returns per-adapter telemetry newest first — `connected`, `disconnected`, `inbound`, `outbound`, `error`, and `lifecycle` records — with `eventType` and `sinceMs` filters. The diagnosis path is: health fields first, then event history, then guardian logs, then restarts.
- **Scheduler**: `list_scheduled_tasks` shows tasks and their recent run results (exit codes, errors). Operators can inspect scheduler runner logs through the guardian CLI.
- **Conversation and host lifecycle**: `list_conversation_events` (area 2).
- **Sandbox processes**: the shell tool itself, plus sandbox process events recorded in the conversation log.

Gap: tool executions are recorded as `tool_requested`/`tool_result` events but there is no aggregated "tool health" view (failure rates, slow tools). REPL/turn-level host logs are visible to the user's terminal but not directly queryable by the agent.

## 4. Sandbox Control

Exo's shell tool runs inside a sandbox, and the agent should have full control over that environment's lifecycle.

By default Exo conversations use `sandboxScope: "agent"`: shell commands share one persistent agent sandbox across conversations, implemented as an agent-owned named reusable sandbox (the owner conversation and sandbox name live in an agent artifact, resolved through `create_sandbox`). A conversation can opt into `sandboxScope: "conversation"` for isolation. The repo mount is part of the sandbox spec, and the default workdir is the first configured mount, so shell inspection starts in Exo's own source tree.

Checkpointing tools:

- `list_sandbox_snapshots` lists snapshots for the selected scope and reports the current snapshot id when known. It reads Exo's snapshot registry (an agent artifact at `config/exo-sandbox-snapshots.json`) — snapshots created through these tools, not every backend snapshot ever made.
- `snapshot_sandbox` captures a filesystem checkpoint.
- `rewind_sandbox` restarts the selected sandbox from a prior snapshot id.

Tool results return the selected scope, sandbox id, owner conversation id, and snapshot id so later actions can target the same environment explicitly. Snapshot/rewind availability depends on the backend: Docker warm sandboxes support the flow; unsupported backends report clear errors. Rewind restores filesystem state only (see area 2 for what it does not roll back).

Gap: there is no sandbox _cloning_ — starting a second sandbox from an existing snapshot while keeping the original running. That is the natural primitive for self-experimentation (try a change in a clone, compare, then adopt) and a prerequisite for cloning the whole agent (area 7).

## 5. Capability Extension: Tools and Adapters

**Tools.** Tools are layered: the TypeScript registry defines what the model can see and call, and a handler either runs in TypeScript or delegates execution to the Rust tool runtime (`crates/executor/src/harness_tool.rs`) via `execution.context.executeTool(...)` with the same function name. Rust-backed tools therefore always need a TypeScript definition; `registerHostTool` in `exo/tools/host-tools.ts` is the standard bridge, and `exo/tools/sandbox-tools.ts` is the reference example. A Rust match arm without a registered TypeScript definition is unreachable, and an agent-installed tool must never reuse the name of a Rust-backed tool.

`inspect_tools` and `manage_tool` are the default discovery and installation path for registry-backed tools. Registry-installed tools load every turn regardless of the legacy compatibility flag. The older `install_agent_tool` / `uninstall_agent_tool` pair and `.exo/agent-tools/` scan are available only when `enableAgentToolCreation` is explicitly enabled. New Rust-backed capability goes through the code-edit path instead (area 1): implement the match arm, register the definition, rebuild, restart.

Local `manage_tool` sources must be workspace-relative. Create them under `/workspace/exo/.exo/tool-sources/<name>` from sandbox `shell`, then install `.exo/tool-sources/<name>`. Absolute paths such as `/tmp/...` belong to the sandbox filesystem and are not valid host-local source paths.

**Skills.** Between prompt text and code-backed tools sits a third extension
surface: durable skills in the standard agent-skills format (`SKILL.md` with
`name`/`description` frontmatter plus instructions and optional bundled text
files). `install_skill`, `list_skills`, `use_skill`, `read_skill_file`, and
`uninstall_skill` manage them; storage is agent artifacts (`skills/index.json`
plus `skills/<name>.json`), so skills persist across conversations and survive
sandbox rewinds, and every install is a versioned, auditable artifact write.
Only names and descriptions are injected each turn; bodies load on demand. See
`docs/design/skills-arch.md`.

**Adapters.** Adapters connect conversations to external systems:

- `create_adapter` creates an enabled adapter record for the current conversation; the adapter supervisor notices it and starts the worker process.
- `list_adapters` shows ids, types, configuration summaries, enabled state, and health fields.
- `disable_adapter` marks an adapter disabled; the worker observes this and exits, preserving history and configuration.
- `delete_adapter` removes the record and stored state for permanent cleanup.
- `send_adapter_message` sends an intentional outbound reply to an external target.

There is no separate `start_adapter`/`stop_adapter`/`restart_adapter`: starting is creating an enabled adapter, stopping is disabling, and `enable_adapter` resumes a disabled adapter without recreating it. The safety boundary: inbound adapter messages wake the conversation, but model text is never automatically posted back externally — `send_adapter_message` is always an explicit decision.

## 6. Prompt Evolution

Exo's behavior is defined by a small set of prompt surfaces it can read (and, through the code-edit path, change):

- `exo/prompts/me.md` — the durable identity prompt: broad behavioral rules such as when to inspect state first, when to prefer reversible operations, and how to think about external side effects.
- `exo/harness.ts` — assembles the developer prompt each turn and describes the available self-control surfaces at a high level.
- `exo/SELF.md` — the compact self map for navigating its own code.
- Tool definitions — `exo/tools/sandbox-tools.ts`, `scheduler-tools.ts`, `guardian-tools.ts`, `introspection-tools.ts`, and `exoharness/typescript/harness/adapter-tools.ts` carry model-visible descriptions and JSON schemas; these are the most direct prompts for when and how to call each tool. `exoharness/typescript/harness/built-in-tools.ts` describes `shell`, `inspect_tools`, `manage_tool`, and the opt-in legacy creation tools.
- `exo/adapters/*/setup-prompt.md` — setup-time guidance for creating specific adapters.
- Wakeup prompts — `crates/executor/src/adapter/runtime.rs` builds the inbound-message wakeup prompts (including the reply-externally contract), and `crates/executor/src/scheduler_runtime.rs` builds scheduled-task wakeup prompts from each task's `reportPrompt`.
- `.exo/exo-profile.md` (or `EXO_LOCAL_PROMPT_FILE`) — local, user-specific instructions that are not checked in.

Checked-in prompts evolve through the normal self-modification loop: edit, commit, rebuild, restart (areas 1 and 8), which makes prompt history auditable in git. The local profile can be edited directly for user-specific memory that should not be committed.

Prompts should keep teaching the same operating model: inspect state before changing it, prefer reversible operations, carry ids from inspection tools into follow-up actions, explain destructive actions before taking them, start self-maintenance from the self map, snapshot before risky experiments, and treat self-restart as a supervisor handoff.

## 7. Cloning and Migration _(not yet built)_

Exo should eventually be able to reproduce itself: spin up a second instance for experimentation, or move itself to another machine or platform. Nothing implements this today; this section sketches the intended shape.

What constitutes "Exo" for cloning purposes (the area 2 inventory):

- **Code**: the git repository at a known commit — already portable.
- **Harness state**: `.exo/exoharness` (agents, conversations, event logs, artifacts), adapter/scheduler records under `.exo`, and the guardian config.
- **Secrets**: API keys and adapter credentials. The hardest part: on macOS they live in the Keychain, so they cannot be copied as files and need explicit export/re-provisioning on the target.
- **Sandbox state**: snapshots of the agent sandbox, which are backend-specific (Docker image/volume vs. apple-container).
- **External identities**: adapter endpoints (Discord bot token, IRC nick, WhatsApp pairing) — single-session by nature, so a clone must either get fresh identities or take over the originals, never share them.

Building blocks that already exist: conversation `fork` in the exoharness API, sandbox snapshots, the adapter records being plain JSON under `.exo`, and the canonical event log for the clone to know its own provenance (a `cloned_from` custom event would make lineage explicit).

The likely path is an export/import pair: a guardian-level `export` action that produces a portable bundle (git ref + `.exo` state + secret manifest listing what must be re-provisioned + snapshot references), and a bootstrap path on the target (`./exo.sh` already encapsulates most platform differences: Docker vs. apple-container, launch mechanics). Migration is then export, transfer, import, re-provision secrets, and re-bind adapter identities — with the old instance drained (area 1's markers) before the new one takes over the external identities.

## 8. Self-Verification and Rollback

Self-modification needs a verification loop, or failed changes accumulate as corruption. Today this exists as practice and primitives rather than as a single tool:

- **Before a change**: `snapshot_sandbox` for filesystem experiments; a clean git state for code changes.
- **Validating a change**: run relevant `cargo`/`pnpm` checks in the sandbox against the repo mount, then call `rebuild_and_restart_exo` to validate, build, and activate the change. Observe the reboot wakeup and event log; operators inspect status and logs through the guardian CLI when needed.
- **Adopting a change**: commit to git; restart services on the new build with the drain/marker flow.
- **Rolling back**: `git revert`/`restore` for code, `rewind_sandbox` for filesystem state, `manage_tool` for registry-installed tools, `disable_adapter` for adapters — while the event log (area 2) preserves the record of what was tried, so the next attempt starts from knowledge rather than amnesia.

Gap worth closing eventually: a canary path — run the changed build against a cloned sandbox or forked conversation (areas 4 and 7) and compare behavior before adopting it on the live instance, instead of validating on the only copy of itself.
