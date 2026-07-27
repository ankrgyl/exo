#!/usr/bin/env bash
# Wait for the in-flight codex_native harbor (pid passed as $1) to finish,
# then run the remaining 3 cells at higher concurrency.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WAIT_PID="${1:?need codex_native harbor pid}"
echo "[relaunch] waiting for codex_native harbor pid=$WAIT_PID to finish..."
while kill -0 "$WAIT_PID" 2>/dev/null; do sleep 30; done
echo "[relaunch] codex_native done; launching remaining 3 cells at conc=$CONC"
docker ps -aq -f status=exited 2>/dev/null | xargs -r docker rm >/dev/null 2>&1
CELLS="codex_over_exo claude_native claude_over_exo" "$HERE/full_run.sh"
