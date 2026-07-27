#!/usr/bin/env bash
# Pre-pull all Terminal-Bench 2.0 task images so a full run hits the local cache
# instead of pulling ~89 images live mid-run. Pulling live unauthenticated can
# hit Docker Hub's pull-rate limit, so run `docker login` first; authenticated
# pulls comfortably cover the whole set.
#
#   ./prepull-images.sh
set -euo pipefail
DATASET="${DATASET:-terminal-bench@2.0}"
CACHE="$HOME/.cache/harbor/tasks"

# Ensure the dataset (and thus each task.toml with its docker_image) is present.
# Run from /tmp so any CWD download artifact doesn't litter this folder.
( cd /tmp && harbor download "$DATASET" >/dev/null 2>&1 ) || true

mapfile -t IMAGES < <(
  grep -rhoE 'docker_image[[:space:]]*=[[:space:]]*"[^"]+"' "$CACHE"/*/*/task.toml 2>/dev/null \
    | sed -E 's/.*"([^"]+)".*/\1/' | sort -u
)
echo "==> ${#IMAGES[@]} task images to ensure cached"
ok=0; pulled=0; fail=0; failed=()
for img in "${IMAGES[@]}"; do
  if docker image inspect "$img" >/dev/null 2>&1; then
    ok=$((ok+1)); continue
  fi
  if docker pull "$img" >/dev/null 2>&1; then
    echo "    pulled $img"; pulled=$((pulled+1)); ok=$((ok+1))
  else
    echo "    FAILED $img"; fail=$((fail+1)); failed+=("$img")
  fi
done
echo "==> done: $ok cached ($pulled newly pulled), $fail failed"
[ "$fail" -gt 0 ] && printf '    failed: %s\n' "${failed[@]}"
exit 0
