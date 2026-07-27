#!/usr/bin/env bash
# Full terminal-bench run for all 4 parity cells, with a memory guard.
#
# Runs the complete terminal-bench@2.0 dataset (89 tasks) for each of:
#   codex_native, codex_over_exo, claude_native, claude_over_exo
# Cells run one at a time (sequentially) to bound the number of concurrent
# Docker sandboxes. A watchdog prunes exited containers and, if free RAM drops
# below FLOOR_MB, aborts the current cell and moves on, so a run can't exhaust
# memory on a small host. Tune CODEX_CONC / CLAUDE_CONC / FLOOR_MB for your machine.
#
# Usage:  ./full_run.sh                      # all 4 cells, full dataset
#         CODEX_CONC=4 CLAUDE_CONC=2 ./full_run.sh
#         CELLS="claude_over_exo" ./full_run.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TB="$HERE/../terminal-bench"
# Per-agent concurrency. Codex (OpenAI) tolerates high concurrency; Claude Code
# is run low because many concurrent claude-code agents share one Anthropic key's
# per-minute TOKEN budget (each turn re-sends a large context) and hit 429s ->
# backoff -> timeouts. conc=3 keeps Claude under the rate limit.
CODEX_CONC="${CODEX_CONC:-8}"      # trials in flight for codex cells
CLAUDE_CONC="${CLAUDE_CONC:-3}"    # trials in flight for claude cells (rate-limit bound)
FLOOR_MB="${FLOOR_MB:-3000}"      # abort a cell if free RAM drops below this (watchdog backstop)
DATASET="${DATASET:-terminal-bench@2.0}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.5}"
CLAUDE_MODEL="${CLAUDE_MODEL:-claude-sonnet-4-6}"
CELLS="${CELLS:-codex_native codex_over_exo claude_native claude_over_exo}"

export PYTHONPATH="$TB${PYTHONPATH:+:$PYTHONPATH}"
export EXO_BUNDLE="${EXO_BUNDLE:-$TB/exo-bundle.tar.gz}"
mkdir -p "$HERE/results"
cd "$HERE"

free_mb(){ free -m | awk '/^Mem:/{print $7}'; }
reap_exited(){ docker ps -aq -f status=exited 2>/dev/null | xargs -r docker rm >/dev/null 2>&1; }
rm_all_task_containers(){ docker ps -aq 2>/dev/null | xargs -r docker rm -f >/dev/null 2>&1; }

run_cell () {
  local name="$1"; local cc="$2"; shift 2
  local log="$HERE/results/full_${name}.log"
  echo "[full] ===== $name (full dataset, conc=$cc, floor=${FLOOR_MB}MB) ====="
  setsid nohup harbor run --dataset "$DATASET" --n-concurrent "$cc" "$@" \
    > "$log" 2>&1 &
  local pid=$!
  echo "[full] $name harbor pid=$pid log=$log"
  # Watchdog loop
  while kill -0 "$pid" 2>/dev/null; do
    reap_exited
    local fm; fm=$(free_mb)
    echo "[full] $(date +%H:%M:%S) $name free=${fm}MB containers=$(docker ps -q | wc -l)"
    if [ "${fm:-99999}" -lt "$FLOOR_MB" ]; then
      echo "[full] !!! ABORT $name: free=${fm}MB < ${FLOOR_MB}MB — killing cell"
      kill -TERM "$pid" 2>/dev/null; sleep 10; kill -KILL "$pid" 2>/dev/null
      rm_all_task_containers
      echo "[full] $name aborted (partial results kept)"
      return 1
    fi
    sleep 20
  done
  reap_exited
  echo "[full] $name done (free=$(free_mb)MB)"
}

for cell in $CELLS; do
  case "$cell" in
    codex_native)
      run_cell codex_native "$CODEX_CONC" -a codex -m "$CODEX_MODEL" \
        --ae "OPENAI_API_KEY=${OPENAI_API_KEY:?}" ;;
    codex_over_exo)
      run_cell codex_over_exo "$CODEX_CONC" --agent-import-path exo_agent.agent:ExoCodexAgent \
        -m "$CODEX_MODEL" --ae "OPENAI_API_KEY=${OPENAI_API_KEY:?}" ;;
    claude_native)
      run_cell claude_native "$CLAUDE_CONC" -a claude-code -m "$CLAUDE_MODEL" \
        --ae "ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:?}" ;;
    claude_over_exo)
      run_cell claude_over_exo "$CLAUDE_CONC" --agent-import-path exo_agent.agent:ExoClaudeCodeAgent \
        -m "$CLAUDE_MODEL" --ae "ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:?}" ;;
    *) echo "[full] unknown cell: $cell" >&2 ;;
  esac
done

echo "[full] ALL CELLS DONE — building table"
python3 "$HERE/report.py" | tee "$HERE/results/FULL_TABLE.txt"
