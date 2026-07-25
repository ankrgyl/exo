#!/usr/bin/env bash
# Groundhog as the canonical event store behind exo — end-to-end demo.
#
# What this shows, in order:
#   1  the log is the only copy: no local event files; restart serves history
#   2  exo's default store is silently editable
#   3  the Groundhog log is not: chain verification catches a flipped byte
#      and reports the exact failure (file_hash_mismatch)
#   4  kernel-bound identity (exo issue #154): a kernel-config change retires
#      the old identity's log and records succession; history spans the seam
#   5  SQL over the agent's whole history
#
# Requirements: a ground-core checkout built at GROUNDHOG_BIN (or the default
# below), and this exo checkout. Everything runs locally; the demo root is
# recreated from scratch on every run.

set -euo pipefail

EXO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
GROUNDHOG_BIN="${GROUNDHOG_BIN:-$HOME/GroundCo/ground-core/target/debug/groundhog}"
# Short root: macOS caps unix-socket paths at ~104 bytes.
DEMO_ROOT="${DEMO_ROOT:-$HOME/.exo-ghdemo}"
ENGINE_DIR="$DEMO_ROOT/engine"
export DEMO_ROOT
export EXO_GROUNDHOG_SOCKET="$ENGINE_DIR/data/ground.sock"
export EXO_GROUNDHOG_KERNEL_CONFIG="$DEMO_ROOT/kernel.toml"

DRIVER=("$EXO_ROOT/target/debug/examples/groundhog_demo")
SERVE_PID=""

banner() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
run()    { printf '\033[2m$ %s\033[0m\n' "$*"; "$@"; }

start_engine() {
  # exec so $! is the engine's own pid, with no wrapper shell in between.
  (cd "$ENGINE_DIR" && exec "$GROUNDHOG_BIN" serve) &>"$DEMO_ROOT/serve.log" &
  SERVE_PID=$!
  echo "$SERVE_PID" >"$DEMO_ROOT/serve.pid"
  until [ -S "$EXO_GROUNDHOG_SOCKET" ]; do sleep 0.1; done
}

stop_engine() {
  if [ -n "$SERVE_PID" ] && kill -0 "$SERVE_PID" 2>/dev/null; then
    kill "$SERVE_PID"
    # Wait for the process to actually exit: offline commands (seal, project)
    # refuse to run while the serve writer lock is held.
    while kill -0 "$SERVE_PID" 2>/dev/null; do sleep 0.1; done
  fi
  # The socket file outlives the process; remove it so the next start_engine
  # waits for the new engine's socket instead of seeing this stale one.
  rm -f "$EXO_GROUNDHOG_SOCKET"
  SERVE_PID=""
}

trap stop_engine EXIT

command -v "$GROUNDHOG_BIN" >/dev/null || { echo "no groundhog binary at $GROUNDHOG_BIN"; exit 1; }
(cd "$EXO_ROOT" && cargo build -q --example groundhog_demo --features basic-backend)

rm -rf "$DEMO_ROOT"
mkdir -p "$ENGINE_DIR"
printf 'mutability = "full"\n' >"$EXO_GROUNDHOG_KERNEL_CONFIG"
(cd "$ENGINE_DIR" && run "$GROUNDHOG_BIN" init >/dev/null)
start_engine

banner "1a · seed a conversation through the exo harness"
run "${DRIVER[@]}" seed

banner "1b · exo wrote no local event files — the log is the only copy"
run find "$DEMO_ROOT/exoharness" -name '*.json' -path '*events*'
echo "(nothing: in Groundhog mode there is no local event store to edit)"

banner "1c · a fresh harness process serves the full history from the log"
run "${DRIVER[@]}" read

banner "2 · for contrast: exo's default store is one editable JSON file per event"
LOCAL_DEMO="$DEMO_ROOT/local-files"
mkdir -p "$LOCAL_DEMO"
DEMO_ROOT="$LOCAL_DEMO" "${DRIVER[@]}" seed-local
NOTE_FILE=$(grep -rl "prefers audited tools" "$LOCAL_DEMO/exoharness" | head -1)
echo "the agent's memory, as exo stores it by default:"
DEMO_ROOT="$LOCAL_DEMO" "${DRIVER[@]}" read-local
run sed -i '' 's/the user prefers audited tools/the user approved the wire transfer/' "$NOTE_FILE"
echo "one sed later, the served history says something else, and nothing noticed:"
DEMO_ROOT="$LOCAL_DEMO" "${DRIVER[@]}" read-local

banner "3a · seal the Groundhog log and verify the whole chain"
stop_engine
(cd "$ENGINE_DIR" && run "$GROUNDHOG_BIN" seal && run "$GROUNDHOG_BIN" verify --chain)

banner "3b · flip one byte in a sealed segment"
SEGMENT=$(ls "$ENGINE_DIR"/data/log/segments/*.parquet | head -1)
run python3 - "$SEGMENT" <<'EOF'
import sys
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
data[len(data) // 2] ^= 0xFF
open(path, "wb").write(data)
print(f"flipped one byte in {path}")
EOF
echo "verification now fails, naming the exact failure:"
if (cd "$ENGINE_DIR" && "$GROUNDHOG_BIN" verify --chain); then
  echo "UNEXPECTED: verify passed on tampered data"
  exit 1
fi

banner "3c · restore the byte; verification passes again"
run python3 - "$SEGMENT" <<'EOF'
import sys
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
data[len(data) // 2] ^= 0xFF
open(path, "wb").write(data)
EOF
(cd "$ENGINE_DIR" && run "$GROUNDHOG_BIN" verify --chain)
start_engine

banner "4a · the kernel contract changes (exo #154's mutability flip)"
run sed -i '' 's/mutability = "full"/mutability = "frozen"/' "$EXO_GROUNDHOG_KERNEL_CONFIG"
echo "the next harness process is a different identity: same agent, new source name"

banner "4b · history is intact across the identity seam, and writing continues"
run "${DRIVER[@]}" read
run "${DRIVER[@]}" append "first note under the frozen kernel"

banner "4c · the old identity's log admits nothing, from anyone"
run "${DRIVER[@]}" retired-append

banner "4d · succession is on the record, validated by the engine"
run "${DRIVER[@]}" lineage

banner "5 · SQL over the agent's whole history"
stop_engine
(cd "$ENGINE_DIR" && run "$GROUNDHOG_BIN" project)
start_engine
run curl -s --unix-socket "$EXO_GROUNDHOG_SOCKET" -X POST http://localhost/v1/query \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT kind, COUNT(*) AS n FROM events GROUP BY kind ORDER BY n DESC, kind"}'
echo

banner "done"
echo "the log survived a restart, a tamper attempt, and a kernel change —"
echo "and the one edit that succeeded was the one nothing verified."
