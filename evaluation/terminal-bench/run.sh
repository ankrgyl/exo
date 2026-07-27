#!/usr/bin/env bash
# Run the exo agent against Terminal-Bench 2.0 via Harbor, then write a report.
#
#   OPENAI_API_KEY=sk-... ./run.sh                # full suite (89 tasks)
#   OPENAI_API_KEY=sk-... ./run.sh -l 5           # first 5 tasks (smoke test)
#   OPENAI_API_KEY=sk-... ./run.sh -t mailman     # a single named task
#   N_CONCURRENT=8 ./run.sh                        # override concurrency
#
# Any extra args are forwarded to `harbor run` (e.g. -l/--n-tasks, -t/--task,
# -i/--include-task-name). Results land in jobs/<timestamp>/; a report is written
# to reports/<timestamp>/ automatically.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

: "${OPENAI_API_KEY:?set OPENAI_API_KEY}"
[ -f exo-bundle.tar.gz ] || { echo "exo-bundle.tar.gz missing — run ./setup.sh first"; exit 1; }

MODEL="${MODEL:-openai/gpt-5.5}"
N_CONCURRENT="${N_CONCURRENT:-4}"
# Agent time budget multiplier. Keep 1.0 for a leaderboard-comparable run (every
# agent gets each task's standard budget); raise only for exploratory "ceiling
# with more time" runs (a few heavy tasks legitimately time out at 1.0).
TIMEOUT_MULT="${TIMEOUT_MULT:-1.0}"

# agent.py is imported as exo_agent.agent:ExoAgent and finds the bundle relative
# to this dir, so PYTHONPATH must include it.
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"

echo "==> harbor run | model=$MODEL concurrency=$N_CONCURRENT timeout_mult=$TIMEOUT_MULT args=$*"
harbor run \
  --dataset terminal-bench@2.0 \
  --agent-import-path exo_agent.agent:ExoAgent \
  -m "$MODEL" \
  --ae "OPENAI_API_KEY=$OPENAI_API_KEY" \
  --n-concurrent "$N_CONCURRENT" \
  --agent-timeout-multiplier "$TIMEOUT_MULT" \
  "$@"

echo "==> Generating report"
"$HERE/.venv/bin/python" "$HERE/gen_report.py"
echo "==> Report written under reports/ (latest job)"
