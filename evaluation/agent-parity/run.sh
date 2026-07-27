#!/usr/bin/env bash
# Agent-integration parity eval.
#
# Thesis: "Exo can integrate existing agents to run as well as they run
# independently." We test it by running the SAME coding agent two ways on the
# SAME Harbor (Terminal-Bench 2.0) tasks and comparing scores:
#
#                  | native (harbor -a <agent>) | over exo (--harness <agent>) |
#   Codex          |                            |                              |
#   Claude Code    |                            |                              |
#
# "Native"   = Harbor's installed agent runs the CLI directly.
# "Over Exo" = exo drives the same CLI via its TypeScript harness runtime
#              (--harness codex / --harness claude-code), shipped into the
#              container by exo_agent.agent:ExoCodexAgent / ExoClaudeCodeAgent.
#
# The four cells share the dataset, the task slice (-l N => same first N tasks),
# the model per agent (gpt-5.5 for Codex, Sonnet for Claude Code), and the
# concurrency. Cells run one at a time to bound the number of concurrent Docker
# sandboxes. (For the full dataset, full_run.sh adds a memory guard.)
#
# Usage:  ./run.sh            # all 4 cells, N=5 tasks
#         N=10 ./run.sh       # 10 tasks
#         CELLS="codex_over_exo" ./run.sh   # just one cell (space-separated)
#
# Requires OPENAI_API_KEY (Codex) and ANTHROPIC_API_KEY (Claude Code) in env.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TB="$HERE/../terminal-bench"            # reuse its exo bundle + agent wrappers

N="${N:-5}"                             # task slice: first N dataset tasks
# Per-agent concurrency: codex (OpenAI) tolerates high; claude run low because
# concurrent claude-code agents share one Anthropic key's per-minute token budget
# and hit 429 rate limits -> backoff -> timeouts.
CODEX_CONC="${CODEX_CONC:-4}"           # trials in flight for codex cells
CLAUDE_CONC="${CLAUDE_CONC:-3}"         # trials in flight for claude cells
DATASET="${DATASET:-terminal-bench@2.0}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.5}"
CLAUDE_MODEL="${CLAUDE_MODEL:-claude-sonnet-4-6}"
CELLS="${CELLS:-codex_native codex_over_exo claude_native claude_over_exo}"

export PYTHONPATH="$TB${PYTHONPATH:+:$PYTHONPATH}"
export EXO_BUNDLE="${EXO_BUNDLE:-$TB/exo-bundle.tar.gz}"
mkdir -p "$HERE/results"
cd "$HERE"                              # harbor writes jobs/ under cwd

run_cell () {
  local name="$1"; local cc="$2"; shift 2
  echo "[parity] ===== $name (N=$N conc=$cc) ====="
  harbor run --dataset "$DATASET" -l "$N" --n-concurrent "$cc" "$@" \
    2>&1 | tee "$HERE/results/$name.log"
}

for cell in $CELLS; do
  case "$cell" in
    codex_native)
      run_cell codex_native "$CODEX_CONC" -a codex -m "$CODEX_MODEL" \
        --ae "OPENAI_API_KEY=${OPENAI_API_KEY:?set OPENAI_API_KEY}" ;;
    codex_over_exo)
      run_cell codex_over_exo "$CODEX_CONC" --agent-import-path exo_agent.agent:ExoCodexAgent \
        -m "$CODEX_MODEL" --ae "OPENAI_API_KEY=${OPENAI_API_KEY:?set OPENAI_API_KEY}" ;;
    claude_native)
      run_cell claude_native "$CLAUDE_CONC" -a claude-code -m "$CLAUDE_MODEL" \
        --ae "ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY}" ;;
    claude_over_exo)
      run_cell claude_over_exo "$CLAUDE_CONC" --agent-import-path exo_agent.agent:ExoClaudeCodeAgent \
        -m "$CLAUDE_MODEL" --ae "ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY}" ;;
    *) echo "[parity] unknown cell: $cell" >&2; exit 2 ;;
  esac
done

echo "[parity] all requested cells done; building table"
python3 "$HERE/report.py"
