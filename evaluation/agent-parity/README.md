# Agent-integration parity eval

**Thesis:** _Exo can integrate existing coding agents to run as well as they run
independently._

We test it head-to-head on [Terminal-Bench 2.0](https://github.com/laude-institute/terminal-bench)
(via Harbor). The same agent CLI is run two ways on the same tasks, and we
compare scores:

|                          | Native                      | Over Exo                    |
| ------------------------ | --------------------------- | --------------------------- |
| **Codex** (gpt-5.5)      | `harbor run -a codex`       | exo `--harness codex`       |
| **Claude Code** (Sonnet) | `harbor run -a claude-code` | exo `--harness claude-code` |

- **Native** — Harbor's installed agent runs the vendor CLI directly.
- **Over Exo** — exo drives the _same_ vendor CLI through its TypeScript harness
  runtime. exo is shipped into the task container as a self-contained bundle and
  configured with a `local-process` sandbox, so exo's shell _is_ the task
  container's shell. Wrappers live in
  [`../terminal-bench/exo_agent/agent.py`](../terminal-bench/exo_agent/agent.py)
  (`ExoCodexAgent`, `ExoClaudeCodeAgent`).

If the "Over Exo" column matches the "Native" column, the integration is
transparent — exo adds the agent without degrading it.

## Run

```bash
export OPENAI_API_KEY=...      # Codex cells
export ANTHROPIC_API_KEY=...   # Claude Code cells

./run.sh                       # all 4 cells, first 5 tasks
N=10 ./run.sh                  # 10 tasks
CELLS="codex_over_exo claude_over_exo" ./run.sh   # subset
```

Cells run one at a time to bound the number of concurrent Docker sandboxes; for
the full dataset use `full_run.sh`, which adds a memory guard.

Concurrency is per-agent: **codex** runs higher (`CODEX_CONC`, default 4/8) while
**claude** runs low (`CLAUDE_CONC`, default 3). Claude must stay low because many
concurrent claude-code agents share one Anthropic key's per-minute token budget
(each turn re-sends a large context) and hit 429 rate limits → backoff →
timeouts; codex is on a separate API and tolerates more.

Knobs (env vars): `N` (task slice), `CODEX_CONC` / `CLAUDE_CONC`, `DATASET`,
`CODEX_MODEL`, `CLAUDE_MODEL`, `CELLS`, `EXO_BUNDLE`.

## Results

```bash
python3 report.py              # rebuild the table from ./jobs
```

Per-cell logs land in `results/<cell>.log`; full Harbor artifacts (per-trial
transcripts, rewards, exo usage) under `jobs/<timestamp>/`.

## Requirements

- `harbor` on PATH (Terminal-Bench 2.0).
- The exo bundle at `../terminal-bench/exo-bundle.tar.gz` (build with
  `../terminal-bench/build-bundle.sh`). The "Over Exo" cells need it; "Native"
  cells do not.
- Docker (Harbor task sandboxes).

## Notes

- The "Over Exo" Codex cell pins `@openai/codex@0.142.3` to match the version
  Harbor's native codex agent installs (so native vs over-exo is the same agent).
  Set in `agent.py` (`ExoCodexAgent._CODEX_VERSION`).
- The "Over Exo" Codex cell also enables codex's built-in `web_search` by writing
  `[tools] web_search = true` to `$CODEX_HOME/config.toml` (native has it on by
  default; exo's app-server thread otherwise only registers its own shell tool).
- `report.py` reads each cell's _latest_ job, so re-running a single cell
  refreshes just that one in the table.
