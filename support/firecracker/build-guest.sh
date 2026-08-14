#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd -P)"
BUILD_ROOT="$REPOSITORY_ROOT/target/firecracker-guest"

case "$(uname -m)" in
  aarch64 | arm64)
    TARGET="aarch64-unknown-linux-gnu"
    CARGO_TARGET_DIR="$BUILD_ROOT" \
      CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" \
      cargo build --manifest-path "$REPOSITORY_ROOT/Cargo.toml" \
        -p exo-firecracker-guest --release --target "$TARGET"
    ;;
  x86_64 | amd64)
    TARGET="x86_64-unknown-linux-gnu"
    CARGO_TARGET_DIR="$BUILD_ROOT" \
      CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" \
      cargo build --manifest-path "$REPOSITORY_ROOT/Cargo.toml" \
        -p exo-firecracker-guest --release --target "$TARGET"
    ;;
  *)
    echo "Unsupported Firecracker guest architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

BINARY="$BUILD_ROOT/$TARGET/release/exo-firecracker-guest"
strip "$BINARY"
if readelf -l "$BINARY" | grep -q INTERP; then
  echo "Firecracker guest runtime is dynamically linked: $BINARY" >&2
  exit 1
fi
printf '%s\n' "$BINARY"
