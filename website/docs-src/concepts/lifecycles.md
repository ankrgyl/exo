---
title: Lifecycles
description: How agents, conversations, sandboxes, and adapters start, stay up, change, and stop.
---

# Lifecycles

Exo's components have different lifetimes and ownership rules. This page
covers the four that matter most in practice:

- [Agents](#agent-lifecycle) — durable identity and config
- [Conversations](#conversation-lifecycle) — sessions, turns, and the event log
- [Sandboxes](#sandbox-lifecycle) — create, start/stop, attach/detach, snapshot
- [Adapters](#adapter-lifecycle) — host workers, supervision, and wakeups

For what each object *is*, see [Data Model](./data-model). For backends and
scope defaults, see [Sandboxes](./sandboxes) and [Adapters](./adapters).

## Mental model

```text
Agent  (long-lived identity + config + secrets)
 └── Conversation  (append-only event log; can pause for days)
      ├── Session  (one live stretch of interaction)
      │    └── Turn  (one user/wakeup input → N model/tool rounds)
      └── Sandbox(es)  (created or attached; optional; scoped agent|conversation)

Adapter runner  (host process; separate from any one turn)
 └── Adapter worker  (one supervised process per enabled adapter)
      └── inbound event → conversation wakeup → normal Turn
```

Durable state lives under `.exo/` on the host. Sandbox filesystems are
separate and can be snapshotted or rewound without erasing the event log.

---

## Agent lifecycle

An **agent** is the top-level identity: display name, slug, executor/harness
config, model binding, sandbox defaults, and agent-scoped artifacts,
bindings, and secrets.

### Create

```bash
exo agent create "My Agent" --model gpt-5.6-terra
# or via the canonical launcher, which creates exo-agent for you:
./exo.sh
```

Creation writes:

- an `AgentRecord` (id, slug, name) under `.exo/exoharness/agents/<id>/`
- executor config as an agent artifact (harness kind, model, sandbox
  image/provider/scope, TypeScript module path, tool modules, …)
- optional agent-scoped bindings and secrets

Nothing is running yet. An agent is configuration plus durable storage
until a conversation runs a turn or a service (adapter runner, scheduler)
touches it.

### Update

```bash
exo agent update <agent> --model <name>
exo agent update <agent> --networking enabled
exo agent update <agent> --sandbox-image ubuntu:24.04
```

Updates rewrite the agent config artifact on disk. **In-process caches may
not reload immediately** — notably the adapter runner can keep using the
config it loaded at startup until that process restarts. After changing
sandbox-related agent config, restart the adapter runner (or use
`guardian_action` / `./exo.sh` service restart) if wakeups still look
stale.

For the **agent-scoped sandbox**, the durable sandbox identity is fixed
once first created. Later agent-config drift does **not** silently replace
that sandbox (so installed packages and filesystem state are preserved).
Applying a new sandbox spec requires explicitly recreating the agent
sandbox.

### Use

Agents are referenced by slug or id from the CLI, REPL, adapters, and
scheduler. Multiple conversations can share one agent. Canonical Exo uses
one primary agent (`exo-agent`) with an agent-scoped sandbox so tools
installed in the sandbox persist across conversations.

### Delete

```bash
exo agent delete <agent>
# bulk local wipe (destructive):
./exo.sh fresh
```

Deleting an agent removes its directory under `.exo/exoharness/agents/`
(record, conversations, agent artifacts, agent-scoped sandbox metadata).
It does **not** by itself tear down every remote provider resource if
something was leaked outside Exo's tracking — prefer stopping/detaching
sandboxes first when using remote backends.

Root-level secrets and bindings are **not** deleted with the agent.

---

## Conversation lifecycle

A **conversation** is a durable, resumable interaction log with an agent.
It can sit idle for a long time and still be the same conversation when
you return.

### Create

```bash
exo conversation create <agent> "Dev"
# canonical setup creates conversation slug `dev` automatically
```

Creation allocates a conversation id/slug, optional conversation-scoped
config (model override, sandbox scope/runtime), and an empty event log.
No model call happens until something sends a turn.

### Sessions and turns

```text
start_session  →  begin_turn(input)  →  (model + tools…)  →  turn.finish()
                 └─ optional: end_session when the live stretch is done
```

| Stage | What happens |
|:------|:-------------|
| **Session** | Groups related turns in one live stretch (one REPL sitting, one adapter wakeup cycle). |
| **Turn** | One user or wakeup input. May include many model rounds and tool calls before `finish()`. |
| **Hot path** | `begin_turn(...)` durably accepts input and returns a **turn handle**. The executor appends messages/tool events through that handle, then finishes. |

Common entry points:

- **REPL / CLI chat** — human messages in an open session
- **`exo conversation send`** — one-shot prompt
- **Adapter wakeup** — inbound external message becomes a normal turn
  (fresh session, closed when the wakeup completes)
- **Scheduler** — completed task can wake the conversation with a compact
  result prompt

Wakeups are serialized per conversation so concurrent adapter/scheduler
events do not interleave turns unsafely.

### Events (source of truth)

Every meaningful change appends to the conversation event log, including:

- `session_started` / `session_ended`
- `turn_started` / `turn_ended`
- `messages`, `tool_requested`, `tool_result`
- `artifact_written`
- `sandbox_created` / `sandbox_started` / `sandbox_stopped` /
  `sandbox_attached` / `sandbox_detached` / `sandbox_snapshotted`
- `conversation_forked`, `conversation_deleted`, plus custom executor events

The **prompt** the model sees is a *view* derived from this log by the
executor — not the log itself. Compaction and memory policy live in the
executor; the raw log remains queryable.

### Fork

```bash
exo conversation fork <agent> <conversation> "Fork Name"
```

Fork branches a **new** conversation from an existing one (optionally up
to a given event). Secrets and bindings remain available; the new
conversation gets its own log from the fork point forward. See
[Time Travel](./time-travel).

### Idle, resume, stop

Conversations do not need a live process. Stopping the REPL or even
`./exo.sh stop-all` leaves conversation history intact under `.exo/`.
Starting again with `./exo.sh` or opening the same conversation resumes
the same log.

Adapter and scheduler wakeups can continue while the TUI is closed (as
long as those host runners are up) — that is how ExoChat keeps working
after `/exit`.

### Delete

```bash
exo conversation delete <agent> <conversation>
```

Appends a `conversation_deleted` marker, then removes the conversation
directory (events, conversation artifacts, conversation-scoped sandbox
records). Agent-level config and other conversations are untouched.

---

## Sandbox lifecycle

A **sandbox** is an isolated (or host-local) execution environment where
commands run. Lifecycle is owned by the exoharness; which sandbox a turn
uses is executor policy (agent scope vs conversation scope, plus
attach/detach).

### States and operations

```text
                    create ─────────────────────────────┐
                       │                                │
                       ▼                                │
                  [ tracked ]                           │
                   /       \                            │
              start         attach (external env)       │
                │                 │                     │
                ▼                 ▼                     │
           [ running ]      [ running, attached ]       │
            /    \               │                      │
     snapshot   stop          detach                    │
        │         │               │                     │
        ▼         ▼               ▼                     │
   snapshot id  [ stopped ]  attachment token           │
        │         │               (env still exists     │
        └──── start (optional    outside Exo)           │
              snapshot_id)                              │
                                                        │
              (records removed with owner delete)  ◄────┘
```

| Operation | Effect |
|:----------|:-------|
| **create** | Provision a new sandbox from image/provider/mounts/network policy. Emits `sandbox_created`. Named creates can reuse an existing matching sandbox. |
| **start** | Ensure the sandbox is running; optional restore from `snapshot_id` and optional provider override (teleport). Emits `sandbox_started`. |
| **run / exec** | Start a process inside the sandbox (`shell` tool, scheduler command, CLI `conversation sandbox run`). |
| **snapshot** | Capture filesystem state; persist payload; emit `sandbox_snapshotted` with a snapshot id. Does **not** checkpoint running processes. |
| **stop** | Stop a **created** (non-attached) sandbox and drop the in-process handle. Emits `sandbox_stopped`. |
| **attach** | Register an **externally created** environment as this conversation's sandbox (currently Docker via container id). Emits `sandbox_attached`. |
| **detach** | Release an attached sandbox back to an attachment descriptor; Exo stops tracking it as active. Emits `sandbox_detached`. Attached sandboxes must be **detached, not stopped**. |

### Create vs attach

**Create** is the default path: Exo provisions the environment from agent
or conversation config (image, mounts, networking, provider).

**Attach** is for bring-your-own environments — e.g. a container started
by any external process (Harbor and similar flows). Exo does not build that
container; it binds an existing one into the conversation and routes
execution there.

Every `exo sandbox` verb names its owner. `--agent` alone makes the agent the
owner; adding `--conversation` makes that conversation the owner instead.
Ownership is not cosmetic: sandbox records live under the owner, so a
conversation cannot address a sandbox its agent owns.

```bash
# Attach an existing Docker container to a conversation
exo sandbox attach --agent <agent> --conversation <conversation> \
  --provider docker \
  --external-id <container-id> \
  --default-workdir /workspace

# Later, hand it back
exo sandbox detach --agent <agent> --conversation <conversation> <exo-sandbox-id>
```

### Which sandbox does a turn use?

Executor selection (simplified):

1. Replay conversation sandbox events into **active candidates**
   (`sandbox_created` / `sandbox_attached`, minus stopped/detached).
2. Prefer the **most recent active attached** sandbox.
3. Else prefer the most recent **created** sandbox that still matches the
   current config-derived spec.
4. Else **create** a new one.
5. If the conversation's sandbox scope is **agent**, use the shared
   agent sandbox (durable name recorded on the agent) instead of a
   per-conversation create — canonical Exo defaults here.

An attached sandbox wins over a normal created one so external workflows
can temporarily own execution without fighting auto-provisioning.

**Creating a conversation-owned sandbox is not enough to make a turn use
it.** The spec match in step 3 compares image, workdir, mounts, durable
filesystems, networking, and an idle timeout of exactly 300s, and
`exo sandbox start` does not produce those defaults — it leaves the workdir
unset, networking on, and its own idle timeout. So a sandbox you started by
hand loses the match and the next turn builds a second one beside it. Bind it
explicitly when you want it used:

```bash
exo sandbox start --agent <agent> --conversation <conversation> \
  --provider docker --image ubuntu:24.04
exo conversation update <agent> <conversation> --sandbox-id <sandbox-id>
```

The binding is recorded as a `sandbox_selected` event and takes priority over
both attachment and spec matching, which is also what makes a sandbox restored
from a snapshot usable when its image no longer matches the configured spec.

**Config changes do not migrate an existing conversation sandbox.** If you
change the sandbox spec mid-flight (image, mounts, networking, provider,
etc.), the next turn that needs a sandbox looks for a candidate matching
the *new* spec. The old sandbox is left as-is (still in history; no longer
the preferred match) and Exo **spins up a fresh sandbox** for the new
spec. Filesystem state is not copied over — install tools again, or
snapshot/restore / attach if you need continuity. (Agent-scoped sandboxes
are different: once created they keep a fixed identity and ignore later
config drift until you explicitly recreate them; see [Agent
lifecycle](#agent-lifecycle).)

### Snapshot and rewind

```text
/snapshot          # REPL: snapshot current sandbox
/snapshots         # list snapshot ids in this conversation
/rewind <id>       # restore filesystem from snapshot (new start from snapshot)
/teleport <prov>   # restore under another provider when supported
```

Agent tools: `snapshot_sandbox`, `list_sandbox_snapshots`, `rewind_sandbox`.

Important distinctions:

- Snapshot/rewind moves **filesystem** state, not the conversation log.
- Conversation fork/rewind is a separate axis ([Time Travel](./time-travel)).
- Snapshots of attached vs created sandboxes depend on backend support;
  treat attach as “execute here,” not necessarily “full snapshot parity.”

### Stop, detach, delete

- **stop** — owned sandboxes Exo created.
- **detach** — sandboxes Exo only borrowed; required instead of stop.
- **delete** — there is no separate long-lived “delete sandbox” user
  command in the common path; sandbox *records* go away when their owning
  conversation or agent is deleted. Remote providers may still need
  explicit cleanup if a race leaked an untracked instance.

### Scope reminder

| Scope | Lifetime intuition |
|:------|:-------------------|
| `agent` | One shared sandbox per agent; survives across conversations; config changes do not auto-recreate it. |
| `conversation` | Sandbox tied to that conversation's event log and candidates. |

Scheduler tasks can also use `task_fresh` — a sandbox reused across that
task's runs only. See [Task Scheduler](./task-scheduler).

---

## Adapter lifecycle

**Adapters** are long-running host-side bridges to external channels
(ExoChat, Discord, WhatsApp, …). They are not tools: tools run inside a
turn; adapters run continuously and *wake* turns.

### Processes

```text
./exo.sh
 ├── adapter runner   (`exo … adapters run --watch`)
 │    └── supervisor per enabled adapter
 │         └── worker process (JSONL stdin/stdout)
 ├── scheduler runner
 └── REPL / TUI  (optional; not required for adapters to work)
```

The **adapter runner** is a single host process (lock-guarded) that:

1. Lists **enabled** adapter records from `.exo/adapters/`
2. Starts a supervisor task per adapter (up to `--limit`)
3. Restarts workers that exit or error (exponential backoff)
4. On drain/restart signals, finishes in-flight work and exits so a
   supervisor (guardian / `./exo.sh`) can bring it back cleanly

Hitting the runner `--limit` or a planned drain is **normal** — the runner
is designed to exit and be restarted rather than grow without bound.

### Adapter record lifecycle

```text
create_adapter  →  enabled worker supervised
       │
       ├─ disable_adapter  →  stop worker, keep history
       │
       └─ delete_adapter   →  remove record + history
```

| Step | What is stored |
|:-----|:---------------|
| **create** | `AdapterRecord` (type, config, secrets by id, enabled=true), optional state dir for pairing data |
| **connect** | Worker reports `connected`; runner updates `last_connected_at_ms` |
| **message in** | Worker → runner → conversation artifact + store event → **wakeup turn** |
| **message out** | `send_adapter_message` enqueues durable outbox → runner writes JSONL to worker when connected |
| **error** | `last_error` on the record; supervisor restarts the worker |
| **disable** | `enabled=false`; supervision stops; history kept |
| **delete** | Record, events, and outbox removed |

Outbound messages are **explicit tool calls**. Model text is never
implicitly mirrored to Discord/WhatsApp/etc.

### Stay-up behavior

- Runner polls for newly enabled adapters on an interval while watching.
- Workers receive config and secrets via environment (`EXO_ADAPTER_*`,
  secret env vars); they should not embed raw tokens in config files.
- After a **guardian restart**, the runner can claim a reboot notice and
  wake conversations so the agent may announce it is back; durable outbox
  delivers once the worker reconnects.
- If the runner is down, the agent goes **deaf** (no inbound wakeups), not
  merely mute — a common operator misread.

### Conversation linkage

Adapters point at an owning conversation (or derive one per target when
`conversationScope` is `target`). Inbound traffic does not invent a
parallel agent loop: it calls the same `send` / wakeup path as a local
user message, with a structured prompt describing adapter id, target,
sender, and reply instructions.

### Operator commands

```bash
# Started automatically by canonical ./exo.sh; or:
exo --harness exo adapters run --watch --limit 50

exo adapters list
# agent tools: create_adapter, list_adapters, disable_adapter,
#              delete_adapter, send_adapter_message
```

Health signals: `last_connected_at_ms`, `last_error` on each record;
runner and worker logs under the paths `./exo.sh` / guardian use for
control logs.

---

## Cross-cutting: what survives what

| If you… | Agents / secrets | Conversation log | Sandbox filesystem | Adapters |
|:--------|:-----------------|:-----------------|:-------------------|:---------|
| `/exit` the TUI | kept | kept | kept (if runners/sandbox still up) | keep running |
| `./exo.sh stop-all` | kept | kept | local containers may stop; remote may idle | stopped |
| rewind sandbox | kept | kept | restored to snapshot | unaffected |
| fork conversation | kept | new branch log | not automatically cloned | unaffected |
| delete conversation | kept | removed | conversation sandbox records removed | adapters may need retarget |
| `./exo.sh fresh` | wiped | wiped | local state wiped | wiped |

This split is intentional: experiments belong in the sandbox and executor;
identity, history, and credentials stay in the exoharness so the agent can
evolve without erasing how it got here.

## Related

- [Data Model](./data-model) — agents, conversations, sessions, turns, events
- [Sandboxes](./sandboxes) — backends and scope
- [Adapters](./adapters) — channel setup and configuration
- [Task Scheduler](./task-scheduler) — recurring work and wakeups
- [Time Travel](./time-travel) — fork and snapshot semantics
- [The Canonical Agent](./canonical-agent) — how `./exo.sh` wires these together
