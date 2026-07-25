#!/usr/bin/env bash
# Harness-level demonstration of Groundhog as exo's canonical conversation-event
# store. The events are synthetic: this script does not invoke a model or execute
# a tool. The kernel-config mismatch behavior is an experimental policy prototype,
# not an implementation of the current scope of exo issue #154.

set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
EXO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
IFS= read -r REQUIRED_GROUNDHOG_REVISION \
  <"$SCRIPT_DIR/groundhog-revision.txt"
STARTUP_TIMEOUT_SECONDS="${STARTUP_TIMEOUT_SECONDS:-10}"
GROUNDHOG_ROOT="${GROUNDHOG_ROOT:-}"

if [ "${#REQUIRED_GROUNDHOG_REVISION}" -ne 40 ]; then
  echo "groundhog-revision.txt must contain one full Git commit hash" >&2
  exit 1
fi

if [ -z "$GROUNDHOG_ROOT" ]; then
  printf 'GROUNDHOG_ROOT must name a Groundhog checkout at revision %s\n' \
    "$REQUIRED_GROUNDHOG_REVISION" >&2
  exit 1
fi

if [ ! -d "$GROUNDHOG_ROOT" ]; then
  echo "GROUNDHOG_ROOT is not a directory: $GROUNDHOG_ROOT" >&2
  exit 1
fi
GROUNDHOG_ROOT="$(cd -- "$GROUNDHOG_ROOT" && pwd)"
GROUNDHOG_BIN="$GROUNDHOG_ROOT/target/debug/groundhog"

case "$STARTUP_TIMEOUT_SECONDS" in
  ''|*[!0-9]*|0)
    echo "STARTUP_TIMEOUT_SECONDS must be a positive integer" >&2
    exit 1
    ;;
esac

if ! ACTUAL_GROUNDHOG_REVISION="$(git -C "$GROUNDHOG_ROOT" rev-parse HEAD 2>/dev/null)"; then
  echo "GROUNDHOG_ROOT is not a readable Git checkout: $GROUNDHOG_ROOT" >&2
  exit 1
fi
if [ "$ACTUAL_GROUNDHOG_REVISION" != "$REQUIRED_GROUNDHOG_REVISION" ]; then
  printf 'Groundhog revision mismatch:\n  required: %s\n  actual:   %s\n' \
    "$REQUIRED_GROUNDHOG_REVISION" "$ACTUAL_GROUNDHOG_REVISION" >&2
  exit 1
fi
if ! git -C "$GROUNDHOG_ROOT" diff --quiet -- || \
   ! git -C "$GROUNDHOG_ROOT" diff --cached --quiet --; then
  echo "Groundhog checkout has tracked modifications; use a clean checkout" >&2
  exit 1
fi

# A new directory prevents deletion or reuse of unrelated data. /tmp also keeps
# the Unix-domain socket path below macOS's short path limit.
if [ -n "${DEMO_ROOT:-}" ]; then
  if [ -e "$DEMO_ROOT" ]; then
    echo "DEMO_ROOT already exists; provide a new path: $DEMO_ROOT" >&2
    exit 1
  fi
  mkdir "$DEMO_ROOT"
else
  DEMO_ROOT="$(mktemp -d /tmp/exo-groundhog-demo.XXXXXX)"
fi
DEMO_ROOT="$(cd -- "$DEMO_ROOT" && pwd)"

ENGINE_DIR="$DEMO_ROOT/engine"
export DEMO_ROOT
export EXO_GROUNDHOG_SOCKET="$ENGINE_DIR/data/ground.sock"
export EXO_GROUNDHOG_KERNEL_CONFIG="$DEMO_ROOT/kernel.toml"

DRIVER=("$EXO_ROOT/target/debug/examples/groundhog_demo")
SERVE_PID=""

banner() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

run() {
  printf '\033[2m$'
  printf ' %q' "$@"
  printf '\033[0m\n'
  "$@"
}

show_server_log() {
  if [ -f "$DEMO_ROOT/serve.log" ]; then
    echo "Groundhog server log:" >&2
    tail -n 40 "$DEMO_ROOT/serve.log" >&2
  fi
}

stop_engine() {
  if [ -n "$SERVE_PID" ] && kill -0 "$SERVE_PID" 2>/dev/null; then
    kill "$SERVE_PID" 2>/dev/null || true
    local attempt=0
    while kill -0 "$SERVE_PID" 2>/dev/null && [ "$attempt" -lt 50 ]; do
      sleep 0.1
      attempt=$((attempt + 1))
    done
    if kill -0 "$SERVE_PID" 2>/dev/null; then
      echo "Groundhog did not stop after 5 seconds; terminating PID $SERVE_PID" >&2
      kill -KILL "$SERVE_PID" 2>/dev/null || true
    fi
    wait "$SERVE_PID" 2>/dev/null || true
  fi
  # Groundhog leaves the socket path behind after a clean stop.
  rm -f -- "$EXO_GROUNDHOG_SOCKET"
  SERVE_PID=""
}

start_engine() {
  (cd -- "$ENGINE_DIR" && exec "$GROUNDHOG_BIN" serve) \
    >"$DEMO_ROOT/serve.log" 2>&1 &
  SERVE_PID=$!
  printf '%s\n' "$SERVE_PID" >"$DEMO_ROOT/serve.pid"

  local attempt=0
  local max_attempts=$((STARTUP_TIMEOUT_SECONDS * 10))
  while [ "$attempt" -lt "$max_attempts" ]; do
    if [ -S "$EXO_GROUNDHOG_SOCKET" ] && kill -0 "$SERVE_PID" 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$SERVE_PID" 2>/dev/null; then
      local status=0
      wait "$SERVE_PID" || status=$?
      SERVE_PID=""
      echo "Groundhog exited before creating its socket (status $status)" >&2
      show_server_log
      return 1
    fi
    sleep 0.1
    attempt=$((attempt + 1))
  done

  printf 'Groundhog did not create %s within %s seconds\n' \
    "$EXO_GROUNDHOG_SOCKET" "$STARTUP_TIMEOUT_SECONDS" >&2
  stop_engine
  show_server_log
  return 1
}

rewrite() {
  local expression="$1"
  local path="$2"
  local temporary="$path.demo-rewrite"
  sed "$expression" "$path" >"$temporary"
  mv -- "$temporary" "$path"
}

trap stop_engine EXIT

for command in cargo curl find git grep mktemp python3 sed tail; do
  command -v "$command" >/dev/null || {
    echo "required command is unavailable: $command" >&2
    exit 1
  }
done

banner "build the pinned Groundhog revision and exo demo driver"
(cd -- "$GROUNDHOG_ROOT" && run cargo build --quiet --locked --bin groundhog)
(cd -- "$EXO_ROOT" && run cargo build --quiet --example groundhog_demo --features basic-backend)

mkdir "$ENGINE_DIR"
printf 'mutability = "full"\n' >"$EXO_GROUNDHOG_KERNEL_CONFIG"
(cd -- "$ENGINE_DIR" && run "$GROUNDHOG_BIN" init >/dev/null)
start_engine

banner "1a · append a synthetic conversation through the exo harness API"
run "${DRIVER[@]}" seed

banner "1b · Groundhog is the only conversation-event copy"
run find "$DEMO_ROOT/exoharness" -name '*.json' -path '*events*'
echo "(no event files; agent and conversation metadata remain in exo's local store)"

banner "1c · a fresh harness process replays the synthetic history"
run "${DRIVER[@]}" read

banner "2 · exo's default event store is editable JSON"
LOCAL_DEMO="$DEMO_ROOT/local-files"
mkdir "$LOCAL_DEMO"
DEMO_ROOT="$LOCAL_DEMO" "${DRIVER[@]}" seed-local
NOTE_FILE=""
while IFS= read -r candidate; do
  NOTE_FILE="$candidate"
  break
done < <(grep -rl "prefers audited tools" "$LOCAL_DEMO/exoharness")
if [ -z "$NOTE_FILE" ]; then
  echo "could not locate the local event file" >&2
  exit 1
fi
echo "before direct file editing:"
DEMO_ROOT="$LOCAL_DEMO" "${DRIVER[@]}" read-local
rewrite \
  's/the user prefers audited tools/the user approved the wire transfer/' \
  "$NOTE_FILE"
echo "after direct file editing, the local store serves the changed content:"
DEMO_ROOT="$LOCAL_DEMO" "${DRIVER[@]}" read-local

banner "3a · seal the Groundhog log and verify its chain"
stop_engine
(cd -- "$ENGINE_DIR" && run "$GROUNDHOG_BIN" seal && run "$GROUNDHOG_BIN" verify --chain)

banner "3b · alter one byte in a sealed segment"
SEGMENTS=("$ENGINE_DIR"/data/log/segments/*.parquet)
if [ ! -f "${SEGMENTS[0]}" ]; then
  echo "Groundhog produced no sealed segment" >&2
  exit 1
fi
SEGMENT="${SEGMENTS[0]}"
run python3 - "$SEGMENT" <<'PYTHON'
import sys

path = sys.argv[1]
with open(path, "rb") as source:
    data = bytearray(source.read())
data[len(data) // 2] ^= 0xFF
with open(path, "wb") as destination:
    destination.write(data)
print(f"altered one byte in {path}")
PYTHON
echo "verification must now report a failure:"
if (cd -- "$ENGINE_DIR" && "$GROUNDHOG_BIN" verify --chain); then
  echo "unexpected result: verification passed after segment alteration" >&2
  exit 1
fi

banner "3c · restore the byte and verify again"
run python3 - "$SEGMENT" <<'PYTHON'
import sys

path = sys.argv[1]
with open(path, "rb") as source:
    data = bytearray(source.read())
data[len(data) // 2] ^= 0xFF
with open(path, "wb") as destination:
    destination.write(data)
PYTHON
(cd -- "$ENGINE_DIR" && run "$GROUNDHOG_BIN" verify --chain)
start_engine

banner "4a · exercise the experimental kernel-config mismatch policy"
rewrite 's/mutability = "full"/mutability = "frozen"/' \
  "$EXO_GROUNDHOG_KERNEL_CONFIG"
echo "the next harness process keeps the same exo agent and uses a successor source"

banner "4b · history remains readable and new writes use the successor source"
run "${DRIVER[@]}" read
run "${DRIVER[@]}" append "synthetic note under the changed kernel config"

banner "4c · Groundhog rejects an append to the retired predecessor"
run "${DRIVER[@]}" retired-append

banner "4d · inspect the recorded successor lineage"
run "${DRIVER[@]}" lineage

banner "5 · query event counts across the synthetic conversation history"
stop_engine
(cd -- "$ENGINE_DIR" && run "$GROUNDHOG_BIN" project)
start_engine
run curl --fail --silent --show-error --unix-socket "$EXO_GROUNDHOG_SOCKET" \
  -X POST http://localhost/v1/query \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT kind, COUNT(*) AS n FROM events GROUP BY kind ORDER BY n DESC, kind"}'
echo

banner "done"
echo "The harness replayed Groundhog-backed synthetic events after a restart,"
echo "Groundhog detected sealed-segment alteration, and the mismatch-policy"
echo "prototype recorded a successor while rejecting writes to its predecessor."
echo "Demo data remains at: $DEMO_ROOT"
