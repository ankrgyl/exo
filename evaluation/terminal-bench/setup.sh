#!/usr/bin/env bash
# One-time setup to run the exo Harbor benchmark on a fresh machine.
#
#   ./setup.sh
#
# Installs the harbor CLI, builds the slim exo bundle, and installs the Python
# deps used by the report scripts. Re-running is safe (idempotent-ish).
#
# This folder lives inside the exo repo; the bundle is built from that repo
# (one level up). Prereqs you must provide yourself:
#   - The exo repo (this is it — one level up).
#   - Rust + the x86_64-unknown-linux-musl target, musl-tools, and pnpm
#     (needed by build-bundle.sh to produce a portable static exo binary).
#   - uv (https://docs.astral.sh/uv/) for the harbor CLI.
#   - Docker (the default Harbor environment runs each task in a container).
#   - OPENAI_API_KEY in your environment when you run ./run.sh.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
EXO_REPO="${EXO_REPO:-$(cd "$HERE/../.." && pwd)}"

echo "==> exo repo: $EXO_REPO"
[ -d "$EXO_REPO" ] || { echo "ERROR: exo repo not found at $EXO_REPO (set EXO_REPO=...)"; exit 1; }

echo "==> Installing harbor CLI (uv tool)"
if ! command -v harbor >/dev/null 2>&1; then
  command -v uv >/dev/null 2>&1 || { echo "ERROR: uv not installed — see https://docs.astral.sh/uv/"; exit 1; }
  uv tool install harbor
fi
echo "    harbor $(harbor --version 2>/dev/null || echo '?')"

echo "==> Python deps for reporting (venv: $HERE/.venv)"
python3 -m venv "$HERE/.venv"
"$HERE/.venv/bin/pip" install -q -r "$HERE/../shared/requirements.txt"

echo "==> Building slim exo bundle from $EXO_REPO"
EXO_REPO="$EXO_REPO" "$HERE/build-bundle.sh" "$EXO_REPO"

echo "==> Done. Set OPENAI_API_KEY and run ./run.sh"
