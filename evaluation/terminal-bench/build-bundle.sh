#!/usr/bin/env bash
# Rebuild the slim exo bundle for Harbor's ExoAgent from the exo repo.
# Run from a recent exo checkout (executor without optimistic-concurrency event
# append; older revisions hit 'turn is stale' on rapid tool-result writes).
#
# Usage: ./build-bundle.sh [EXO_REPO]   (default: the exo repo this folder lives in)
# Requires: rust + the x86_64-unknown-linux-musl target (musl-tools), pnpm.
set -euo pipefail
# evaluation/ lives inside the exo repo, so the repo root is one level up.
EXO="${1:-${EXO_REPO:-$(cd "$(dirname "$0")/../.." && pwd)}}"
OUT="$(cd "$(dirname "$0")" && pwd)/exo-bundle.tar.gz"
cd "$EXO"

# Ensure deps + a static musl binary exist (portable across task-image glibc versions).
[ -d node_modules ] || pnpm install --frozen-lockfile
cargo build --release --target x86_64-unknown-linux-musl -p exo

# Vendor a Node binary into the bundle so install() needs NO internet (some task
# sandboxes have no network). The bundled node is glibc x86_64; install() falls
# back to in-image / NodeSource node on old-glibc images where it won't run.
NODE_VER="${NODE_VER:-v22.14.0}"
CACHE="$(dirname "$OUT")/.cache"
mkdir -p "$CACHE"
if [ ! -x "$CACHE/node" ]; then
  echo "fetching node $NODE_VER"
  curl -fsSL "https://nodejs.org/dist/$NODE_VER/node-$NODE_VER-linux-x64.tar.xz" \
    -o "$CACHE/node.tar.xz"
  tar xf "$CACHE/node.tar.xz" -C "$CACHE"
  cp "$CACHE/node-$NODE_VER-linux-x64/bin/node" "$CACHE/node"
fi

tar czf "$OUT" \
  -C "$CACHE" node \
  -C "$EXO" \
  --exclude='node_modules/.pnpm/@img*' --exclude='node_modules/.pnpm/sharp*' \
  --exclude='node_modules/.pnpm/sodium-native*' \
  --exclude='node_modules/.pnpm/@discordjs*' --exclude='node_modules/.pnpm/discord.js*' \
  --exclude='node_modules/.pnpm/@whiskeysockets*' --exclude='node_modules/.pnpm/baileys*' \
  --exclude='node_modules/.pnpm/@cursor*' \
  --exclude='node_modules/.pnpm/playwright*' --exclude='node_modules/.pnpm/@playwright*' \
  --exclude='node_modules/.pnpm/oxlint*' --exclude='node_modules/.pnpm/@oxlint*' --exclude='node_modules/.pnpm/oxfmt*' \
  --exclude='node_modules/.pnpm/rolldown*' --exclude='node_modules/.pnpm/@rolldown*' \
  --exclude='node_modules/.pnpm/@typescript+native-preview*' \
  --exclude='node_modules/.pnpm/vitest*' --exclude='node_modules/.pnpm/@vitest*' \
  --exclude='node_modules/.pnpm/prism-media*' --exclude='node_modules/.pnpm/opusscript*' \
  --exclude='node_modules/.cache' \
  target/x86_64-unknown-linux-musl/release/exo package.json pnpm-lock.yaml tsconfig*.json typescript examples/typescript examples/simple-coding-agent node_modules
echo "wrote $OUT ($(du -sh "$OUT" | cut -f1))"
