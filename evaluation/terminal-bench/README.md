# Terminal-Bench 2.0 — Exo evaluation

Runs Exo as an agent on
[Harbor](https://github.com/harbor-framework/harbor)'s **Terminal-Bench 2.0**
(89 terminal/coding tasks) and produces a scored report. Lets Exo be measured on
the same public benchmark and leaderboard as Claude Code / Codex / OpenHands,
with no bespoke scoring.

## How Exo plugs into Harbor

Harbor runs an _agent_ against an isolated _environment_ (a Docker container per
task). Exo is wrapped as a Harbor **installed agent** (`exo_agent/agent.py`,
`ExoAgent(BaseInstalledAgent)`):

- **`install()`** ships a slim, self-contained exo bundle into the task container
  (a static-musl `exo` binary + pruned `node_modules` + the harness sources) and
  puts `exo` on `PATH`. The static binary runs on any task image regardless of
  its glibc.
- **`run()`** registers the model, creates a conversation on Exo's minimal
  **Simple Coding Agent** harness with a **`local-process` sandbox**, mounts the
  task working dir, and delivers the task via `exo conversation send`.

The key trick: Harbor expects the agent to act on the container, while Exo runs
its own sandbox. Using Exo's `local-process` sandbox _inside_ the container makes
**Exo's shell == the task container's shell** — no nested sandbox and no changes
to Exo. The agent itself is the Simple Coding Agent (a single `shell` tool + a
verify-before-finish system prompt; source in
`../../examples/simple-coding-agent/`), so the score reflects the agent loop and
the model, not extra scaffolding.

## Prerequisites

- The **exo repo** (this folder lives inside it; the bundle is built from one
  level up — override with `EXO_REPO=/path/to/exo`). Build from an exo whose
  executor does not use optimistic concurrency on event append, or runs hit
  `turn is stale` on rapid tool-result writes.
- **Rust** + the `x86_64-unknown-linux-musl` target and `musl-tools` (for the
  portable static exo binary).
- **pnpm** (exo's JS deps), **uv** (the harbor CLI), **Docker**, **Python 3**.
- An **`OPENAI_API_KEY`** (must be completion/Responses-capable — a read-only key
  401s).

## Quickstart

```bash
# 1. One-time setup: install the harbor CLI, build the exo bundle, install report deps.
./setup.sh

# 2. Run the benchmark (writes jobs/<ts>/ and a report under reports/<ts>/).
OPENAI_API_KEY=sk-... ./run.sh              # full 89-task suite
OPENAI_API_KEY=sk-... ./run.sh -l 5         # smoke test: first 5 tasks
OPENAI_API_KEY=sk-... ./run.sh -i mailman   # one named task
```

`run.sh` runs `harbor run -d terminal-bench@2.0 --agent-import-path
exo_agent.agent:ExoAgent -m openai/gpt-5.5`, forwards extra args to `harbor run`
(`-l/--n-tasks`, `-t/--task`, `-i/--include-task-name`, …), then generates a
report. Tune with `MODEL=`, `N_CONCURRENT=`, and `TIMEOUT_MULT=` env vars.

## What's here

| Path                  | What                                                                       |
| --------------------- | -------------------------------------------------------------------------- |
| `exo_agent/agent.py`  | `ExoAgent` — the Harbor installed-agent wrapper for Exo.                    |
| `build-bundle.sh`     | Builds the slim static exo bundle (`exo-bundle.tar.gz`) from the exo repo.  |
| `setup.sh` / `run.sh` | One-time setup; run the suite + report.                                     |
| `gen_report.py`       | Scoreboard + per-task report from a `jobs/<ts>/` run → `reports/<ts>/`.     |
| `ran_graphs.py`       | Extra graphs over executed-only tasks (calls/cost/reward).                  |

## Reports (manual)

`run.sh` calls `gen_report.py` automatically. To regenerate by hand:

```bash
.venv/bin/python gen_report.py [jobs/<ts>]     # default: latest jobs/ dir
.venv/bin/python ran_graphs.py <report-ts>     # executed-only graphs
```

## Not committed (regenerated locally)

`.gitignore` excludes downloads/outputs: `exo-bundle.tar.gz` (built by
`setup.sh`), per-run `jobs/`, and generated `reports/`. The bundle is rebuilt
from your exo checkout; everything else regenerates per run.
