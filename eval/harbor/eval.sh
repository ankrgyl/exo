#!/usr/bin/env bash
set -euo pipefail

eval_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$eval_dir/../.." && pwd)
venv="$eval_dir/.venv"

if [[ ! -x "$venv/bin/python" ]]; then
  python3.12 -m venv "$venv"
fi

if ! "$venv/bin/python" -c 'import harbor, exo_harbor' 2>/dev/null; then
  "$venv/bin/pip" install -e "$eval_dir"
fi

command=("$venv/bin/python" "$eval_dir/eval.py" "$@")

# Harbor runs each benchmark task in a Docker container. Most shells can use
# Docker directly; this workspace needs its configured docker group activated.
if docker info >/dev/null 2>&1; then
  cd "$repo_root"
  exec "${command[@]}"
fi

if command -v sg >/dev/null 2>&1 && getent group docker >/dev/null 2>&1; then
  printf -v quoted_command '%q ' "${command[@]}"
  cd "$repo_root"
  exec sg docker -c "$quoted_command"
fi

echo "Docker is unavailable. Start Docker or grant this user Docker access." >&2
exit 1
