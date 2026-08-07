#!/usr/bin/env bash
# Boots the PyBoy emulator sidecar (foreground). The agent side runs through
# the exo CLI — see README.md.
#
#   ./run.sh                 # uses first ROM in roms/*.gb
#   ./run.sh --rom roms/pokemon-red.gb
#
# Requires: python3 (venv).
set -euo pipefail
cd "$(dirname "$0")"

ROM=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rom) ROM="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

if [[ -z "$ROM" ]]; then
  ROM=$(ls roms/*.gb roms/*.gbc 2>/dev/null | head -1 || true)
fi
if [[ -z "$ROM" || ! -f "$ROM" ]]; then
  echo "No ROM found. Put a Pokemon Red/Blue .gb file in roms/ (gitignored)." >&2
  exit 1
fi

VENV=venv
if [[ ! -x "$VENV/bin/python" ]]; then
  echo "creating venv + installing pyboy (one-time)..."
  python3 -m venv "$VENV"
  "$VENV/bin/pip" install -q -r emulator/requirements.txt
fi

PORT="${GAMEBOY_EMULATOR_PORT:-8777}"
HOST="${GAMEBOY_EMULATOR_HOST:-127.0.0.1}"
cat <<NEXT

Emulator starting on http://$HOST:$PORT
Watch the game live at http://$HOST:$PORT/view

Next, in another terminal (from the repo root), make the agent play:

  exo secret set openai --env OPENAI_API_KEY        # once
  exo model register gpt-5.5 --secret openai        # once
  exo --harness typescript agent create "Gameboy" \\
    --module exoharness/examples/gameboy-agent/agent/harness.ts \\
    --model gpt-5.5 --max-tool-round-trips 20       # once
  exo conversation create gameboy "Play Pokemon"    # once

  exo conversation send gameboy play-pokemon "Play Pokemon Red. Get through the intro and pick a starter."

Each send plays one turn; repeat (or loop) to keep playing.

NEXT
exec "$VENV/bin/python" emulator/server.py --rom "$ROM" --port "$PORT" --host "$HOST"
