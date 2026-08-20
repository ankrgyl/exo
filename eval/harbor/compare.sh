#!/usr/bin/env bash
set -euo pipefail

eval_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
venv="$eval_dir/.venv"

if [[ ! -x "$venv/bin/python" ]]; then
  python3.12 -m venv "$venv"
fi

if ! "$venv/bin/python" -c 'import harbor, exo_harbor' 2>/dev/null; then
  "$venv/bin/pip" install -e "$eval_dir"
fi

exec "$venv/bin/python" "$eval_dir/compare.py" "$@"
