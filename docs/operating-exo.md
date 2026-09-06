# Operating Exo

This document describes Exo from an operator's point of view: what processes run on your
machine, where state lives, and how to debug the stack when something misbehaves. For the
conceptual model (agents, conversations, tools, adapters), read [EXO-BASICS](EXO-BASICS.md)
first; this document assumes those concepts and focuses on the running system.

## What starts when you run exo.sh

`./exo.sh` lives at the repository root (it was previously under `scripts/`). Run with no
arguments it starts the full canonical stack and drops you into a chat REPL. In order:

1. Builds the `exo` binary with cargo if it is missing — only if missing, so after editing
   `crates/` run `./exo.sh build` to pick up changes. The scheduler runner is rebuilt both
   when missing and when its sources are newer than the binary.
2. Ensures the sandbox image is present (pulling it if needed), and ensures the default
   agent and conversation exist. These steps are idempotent; existing records are reused.
3. Mounts the Exo repository into the agent sandbox read-write at `/workspace/exo`, so the
   agent can inspect and modify its own source.
4. Writes the guardian configuration to `.exo/exo-service-guardian.env` (configuration only;
   no guardian process is started).
5. Starts the **scheduler** in the background: `exo-scheduler-runner run --watch
--interval-seconds 10`, detached with `nohup`, logging to `.exo/exo-scheduler.log`,
   pid in `.exo/exo-scheduler.pid`.
6. Starts the **adapter runner** in the background: `exo adapters run --limit 50` with a
   lock file, drain marker, and reboot-notice path under `.exo/`, logging to
   `.exo/exo-adapters.log`, pid in `.exo/exo-adapters.pid`.
7. Runs the **REPL** in the foreground, wired to the default agent and conversation, with
   the scheduler and adapter logs tailed into the same terminal.

Every `exo` invocation from the script passes `--env-file-if-exists .env` and
`--sandbox-backend`; `--harness exo` is passed only at agent creation and adapter runner
startup. The default agent uses `examples/exo/harness.ts`, image `ubuntu:24.04`,
networking enabled, and agent-scoped sandboxing. Nothing requires root or `sudo`.

Templates change the shape of the stack: `canonical` (default) uses Docker sandboxes and
ExoChat; `--template dev` swaps in IRC and Discord adapters. `--template minimal` does
_not_ disable the stack — sandbox, scheduler, and adapter runner still start by default.
It skips the Docker backend defaults (the platform default backend is used instead),
adapter setup prompts, the control console (log tailing), guardian configuration, and
automatic image pulls (a missing sandbox image is then a startup error). A truly bare
REPL needs explicit `--no-sandbox --no-scheduler --no-adapters`.

Exiting the REPL (`/exit`) intentionally leaves both runners going in the background —
the agent stays reachable through its adapters. `./exo.sh stop-all` stops the two
runners, preserving `.exo` and leaving warm containers running; a bare `./exo.sh` later
resumes with state intact. `./exo.sh fresh` rebuilds, deletes agent/adapter state,
removes exo sandbox containers whose owner is dead or that mount this checkout, and
starts over.

There is no watchdog. If a background process crashes, nothing restarts it automatically:
recovery happens the next time you run `./exo.sh` (which detects missing processes and
restarts them, and also restarts processes whose binary or adapter sources are newer than
their pid file) or when someone runs a guardian command.

## Host processes

| Process         | Command                                                   | Role                                                                                                                                                                                                                                                                         |
| --------------- | --------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| REPL            | `exo repl`                                                | Interactive chat in the foreground. Creates the agent/conversation on demand if they do not exist.                                                                                                                                                                           |
| Scheduler       | `exo-scheduler-runner run --watch`                        | Claims due scheduled tasks each interval (up to `--limit` per cycle, default 10) and runs them concurrently. Without `--watch` it does one pass and exits, which suits cron.                                                                                                 |
| Adapter runner  | `exo adapters run`                                        | Supervises adapter worker processes. `--limit` is the maximum number of adapters supervised concurrently, not a run count; the runner loops forever and only exits when its drain marker is claimed or on error.                                                             |
| Adapter workers | `pnpm tsx examples/exo/adapters/<type>/worker.ts`         | One host process per enabled adapter, spawned by the adapter runner. Speaks newline-delimited JSON: events on stdout, commands (outbound sends) on stdin, free-form logging on stderr.                                                                                       |
| Harness runner  | `node --import tsx typescript/harness/runner.ts <module>` | Persistent sidecar that runs the TypeScript harness module. Each host process that executes turns (REPL, scheduler, adapter runner) lazily spawns its own runner per module and talks to it over stdin/stdout NDJSON.                                                        |
| `exo serve`     | optional                                                  | Headless HTTP transport for exoharness storage primitives, by default on `127.0.0.1:4766` (loopback). It is not an executor or a model API; other `exo` processes can point `--exoharness-url` at it to share one `.exo` store. Model execution stays in the client process. |

Single-instance protection: the scheduler and adapter runner each hold a pid-bearing lock
file (`.exo/exo-scheduler.lock`, `.exo/exo-adapters.lock`). A second copy refuses to start
while the pid is alive; a stale lock from a dead pid is reclaimed automatically.

Graceful restart: writing the drain marker (`.exo/exo-adapters.restart`,
`.exo/exo-scheduler.restart`) asks the runner to finish in-flight work and exit cleanly.
The guardian's stop/restart commands do this first and only kill the process tree if the
marker is not claimed in time. Prefer this over killing a runner mid-turn.

If a single adapter worker crashes, the runner restarts it with exponential backoff
(5s doubling to a 300s cap, reset after 60s of stable operation). If the whole runner
process dies, see "no watchdog" above.

## Sandboxes and Docker

Sandbox containers are created lazily, before the first turn of a conversation, and kept
warm: `docker run --detach --name exo-<hash>-<gen> ... sleep infinity`. There is no Exo
daemon inside a container; PID 1 is `sleep infinity`. What actually runs inside:

- the `shell` tool (`bash -lc <command>` via `docker exec`),
- long-lived sandbox processes started by harnesses,
- scheduled task commands.

Everything else runs on the host: the `shell` plumbing and scheduler/adapter tools are
Rust functions in the host process; `install_agent_tool`, `uninstall_agent_tool`,
TypeScript tool modules, and agent-created tools execute in the host harness-runner node
process; adapter workers are host processes. The isolation boundary covers shell
commands and sandbox processes, not tool code.

Containers are used for: filesystem isolation with an explicit mount allowlist, a network
policy (no network, or a dedicated `exo-default` network), a reusable agent environment
shared across conversations, and filesystem snapshot/rewind.

Scoping: with agent scope (the canonical default) one container is shared by all of an
agent's conversations, and its identity is pinned in a durable record
(`config/agent-sandbox-v2.json`) — changing the agent's sandbox config does not silently
evict the container; recreation must be explicit. With conversation scope each
conversation gets one container per sandbox spec, so changing the image or mounts creates
a new container and the old one is reaped when idle. Warm containers survive process
restarts and are reclaimed across processes via `exo.sandbox.key` and
`exo.sandbox.spec-hash` labels. Containers idle for more than 300 seconds are _removed_
(`docker rm -f`), not stopped — lazily, at the next sandbox acquisition rather than on a
timer. Anything written outside mounted paths (installed packages included) is lost at
that point unless snapshotted. Each exec is preceded by a health check, and a dead
container is rebuilt in place.

`--sandbox-backend` (env `EXO_SANDBOX_BACKEND`) selects among `docker`,
`apple-container`, and `local-process`. Outside of `exo.sh`, the default is
`apple-container` on macOS and `docker` elsewhere; `exo.sh`'s canonical template pins
`docker`. `local-process` runs commands directly on the host with no isolation. Among the
local backends, only Docker supports snapshot and rewind (`docker commit`/`save`/`load`);
the other two refuse with an explicit error.

## How the pieces relate

```
                          host                                    containers
  ┌───────────────────────────────────────────────────┐   ┌─────────────────────┐
  │  exo repl ─────────┐                               │   │  agent sandbox      │
  │                    │                               │   │  (docker, warm,     │
  │  exo-scheduler-    ├──> harness runner (node/tsx)  │   │   sleep infinity)   │
  │  runner --watch ───┤     runs harness module +     │   │                     │
  │                    │     TS tools; NDJSON stdio ───┼───┼─> shell / sandbox   │
  │  exo adapters run ─┘                               │   │   processes via     │
  │        │                                           │   │   docker exec       │
  │        ├──> worker.ts (exochat)  <── chat surface  │   └─────────────────────┘
  │        ├──> worker.ts (discord/irc/...)            │
  │        │      stdout events / stdin commands       │
  │        v                                           │
  │   .exo/  (events, artifacts, secrets, adapters,    │
  │          scheduled-tasks, pid/lock/log files)      │
  └───────────────────────────────────────────────────┘
```

The REPL, scheduler, and adapter runner are peer host processes sharing the same `.exo`
store. Each embeds its own executor and spawns its own harness-runner subprocess to run
turns. Inbound adapter messages wake the target conversation for a turn; outbound agent
messages go through a durable outbox that the runner flushes to the worker's stdin and
confirms on acknowledgement.

## State under .exo

The state root is `--root`, default `.exo` relative to the working directory. Everything
durable lives here as plain JSON files (the one exception: snapshot payloads are binary
`payload.bin` blobs):

- `.exo/exoharness/` — the object store. Per agent:
  `agents/<id>/record.json`, per-conversation `events/` (**the event ledger** — one JSON
  file per event, uuid7-ordered; all messages, tool calls, and sandbox records),
  `artifacts/` (including `config/executor.json`, the agent configuration itself, and the
  agent's durable memory), `bindings/`, `secrets/`, `sandboxes/`, `snapshots/`. Root-level
  `bindings/` hold model registrations; root-level `secrets/` hold encrypted API keys.
- `.exo/scheduled-tasks/` — `tasks/<id>.json` and `runs/<task>/<run>.json`.
- `.exo/adapters/` — adapter registrations, event history, outbox, plus per-adapter worker
  state directories (e.g. messaging-platform pairing credentials and sessions).
- Runtime files: `exo-scheduler.{pid,lock,log}`, `exo-adapters.{pid,lock,log}`,
  `*.restart` drain markers, `exo-reboot-notice.json`, `exo-service-guardian.env`,
  `exo-profile.md`.

Conversation event appends are protected by optimistic concurrency against the
conversation head pointer, so the event ledger is load-bearing, not just a log.
Conversation-scope sandbox reuse is additionally derived from `sandbox_created` events;
agent-scope sandbox identity lives in the `config/agent-sandbox-v2.json` artifact instead.

## Secrets

Secrets are stored encrypted (AES-256-GCM) as JSON files in the store; metadata (name,
type, timestamps) is plaintext, values are ciphertext. Set them with
`exo secret set <name> --env <VAR>` so the value never appears in argv.

The master key lives in one of two backends (`--secret-backend` / `EXO_SECRET_BACKEND`):
`apple-keychain` (macOS default) keeps a 32-byte key in the Keychain; `file` (default
elsewhere) keeps it at `~/.config/exo/master.key` (0600), overridable with
`EXO_MASTER_KEY_PATH`. Note that `EXO_MASTER_KEY_PATH` and `EXO_SECRET_BACKEND` are read
by the `exo` CLI only; the scheduler runner uses the platform default backend.

How workers receive secrets:

- **LLM API keys stored via `exo secret set`** never enter a subprocess environment: the
  harness runner requests them over its stdio protocol at turn time and the host returns
  the decrypted value over the pipe. Caveat: `.env` variables _are_ forwarded into the
  harness runner's environment, so a key placed in `.env` does reach that subprocess.
- **Adapter secrets** are resolved from the store at worker start and injected into the
  worker process as environment variables, per the adapter's `secret_env` configuration,
  alongside `EXO_ADAPTER_ID`, `EXO_ADAPTER_TYPE`, `EXO_ADAPTER_STATE_DIR`, and
  `EXO_ADAPTER_CONFIG`.

If the master key is lost or replaced, a new key is generated silently and every existing
secret becomes undecryptable — the symptom is that secrets still list fine but all
decryption fails. The only recovery is re-setting each secret from the original values.

## Logs

- `.exo/exo-scheduler.log` — one line per scheduled task run (task id, run id, exit,
  error).
- `.exo/exo-adapters.log` — the adapter runner's output. Worker stderr is inherited into
  this file, so most useful content here is per-worker logging (workers prefix their
  lines, e.g. `[whatsapp-adapter]`).
- `.exo/exo-service-guardian-actions.log` — output of deferred guardian actions.
- The REPL logs to the terminal only; in the default mode it also tails the two logs above
  with `[scheduler]` / `[adapters]` prefixes.
- The harness runner has **no log file**: its stdout is the protocol channel and its
  stderr is buffered in memory and attached to the error message only if the process dies.
  A stray non-protocol line on stdout (e.g. `console.log`) is dropped by the host, or
  fails the pending call if it carries an id — debug output from harness or adapter code
  must go to stderr.
- Output of processes run inside sandboxes is persisted as conversation events
  (`sandbox_process_event`), inspectable with `exo conversation events`.
- `exo serve` is silent unless run with `-v`/`-vv`.

Known gap: internal `tracing` log statements in the adapter runner and scheduler have no
subscriber installed, so they do not reach any log file. The conversation event log and
the adapter event store are the more reliable sources of truth.

## What is safe to delete

Safe (recreated automatically or re-derivable):

- pid, lock, and log files; `*.restart` markers; `exo-reboot-notice.json`
- `exo-service-guardian.env` (rewritten on the next `./exo.sh` launch)

One file deserves a warning even though nothing breaks without it: `.exo/exo-profile.md`
is operator-authored prompt customization. It is loaded if present but never regenerated —
treat it as data, not cache.

- `.exo/adapters/media/` (outbound attachment staging)
- snapshot payloads (you lose the ability to rewind to them, nothing else)
- root-level `secrets/` and `bindings/` — but only if you still have the original API keys
  to re-run `exo secret set` and `exo model register`

Not safe (irrecoverable, or breaks invariants):

- `exoharness/.../conversations/.../events/` — the entire conversation history, and the
  data conversation-scope sandbox reuse is derived from
- `exoharness/agents/<id>/artifacts/` — includes the agent configuration itself and the
  agent-scope sandbox identity record; deleting it makes the agent fail to start, and
  also destroys the agent's durable memory
- the master key (Keychain entry or `master.key` file) — see Secrets above
- anything inside `agents/` structure by hand; slug markers and directories must stay
  consistent, so use `exo agent delete` instead
- per-adapter worker state under `.exo/adapters/<type>/` for adapters with device pairing —
  deleting it forces re-pairing (QR code) on next start

`./exo.sh stop-all` preserves all of `.exo`. `./exo.sh delall all` and `fresh` delete
agent, conversation, and adapter state deliberately.

## Health and troubleshooting

Where to look, in rough order:

1. `./exo.sh list` — agents and conversations; `exo adapters list` — adapter status.
2. `examples/exo/scripts/exo-service-guardian status` — running/stopped for scheduler and
   adapter runner with pids and file paths; `... logs [scheduler|adapters]` tails logs.
   The agent can invoke the same surface via the `guardian_action` tool.
3. `exo conversation events <agent> <conversation> [--type ...] [--limit N]` — the core
   debugging command: every message, tool call, error, and host lifecycle event.
4. `exo agent show`, `exo model list`, `exo secret list` for configuration checks.
5. Check pid files under `.exo/` against `ps` when in doubt about whether a runner is up.

Common failure modes:

- **The agent goes "deaf": inbound messages get no reaction, but the CLI and outbound
  sends work.** First check whether the adapter runner process is alive. The runner exits
  when its drain marker is claimed, on unrecoverable errors, or when killed — and nothing
  restarts it until the next `./exo.sh` run or a guardian `start-services`. Symptom-wise
  this is indistinguishable from a slow or broken model, so check the process first.
- **A turn "produces nothing".** Distinguish a dead runner from a quiet model via
  `exo conversation events`: if the events show a turn ran but emitted no text and no
  tool call, the model returned an empty completion (some models do, in practice) — the
  stack itself is healthy.
- **Boot-time ordering under a service manager.** If you start the stack automatically at
  boot (launchd/systemd), the Docker daemon may come up after the runners — sandbox
  creation then fails until Docker is ready, so gate startup on Docker availability.
  Service-manager environments are also nearly empty: set `PATH` explicitly (the runners
  need the pinned node/pnpm toolchain — a wrong node on `PATH` makes harness turns crash
  on missing Web API globals) and any Docker-related variables your setup needs.
- **Secrets hang or prompt in unattended contexts.** On macOS the default secret backend
  is the Keychain, which can require interactive authorization; headless or
  service-manager deployments should use `--secret-backend file` (with
  `EXO_MASTER_KEY_PATH` if a custom location is needed). Remember the scheduler runner
  follows the platform default backend.
- **An adapter reconnects in a loop.** Worker crashes are retried with backoff and
  recorded in `.exo/adapters/events/<id>/` and the adapters log. For adapters holding
  long-lived network connections, check DNS and proxy interference on the host (VPNs and
  fake-IP DNS interception can make name resolution fail persistently for the worker) —
  give the relevant domains a direct route if so. Also note the worker protocol: any
  non-JSON line on a worker's stdout kills and restarts its loop, so worker debug output
  must go to stderr.
- **Restart requested but nothing happens.** Drain markers are only claimed between loop
  iterations; a long in-flight turn or task delays the restart until it finishes or the
  guardian's timeout force-kills the process. This is expected — prefer waiting over
  killing a runner mid-turn.
- **"already appears to be running with pid X."** The lock file is held by a live process;
  stop it first (locks from dead pids are reclaimed automatically).
- **Docker missing or down.** Startup fails with an explicit message; at runtime the
  docker backend reports that the `docker` CLI is required. Snapshot/rewind on
  `apple-container` fails with an error suggesting the Docker backend; on `local-process`
  it fails as plainly unsupported.
- **All secrets fail to decrypt at once.** The master key changed — see Secrets.
