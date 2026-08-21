#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 --guest-runtime <path> --output <initramfs.cpio>" >&2
}

GUEST_RUNTIME=""
OUTPUT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --guest-runtime)
      GUEST_RUNTIME="$2"
      shift 2
      ;;
    --output)
      OUTPUT="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$GUEST_RUNTIME" || -z "$OUTPUT" ]]; then
  usage
  exit 2
fi
if [[ ! -x "$GUEST_RUNTIME" ]]; then
  echo "Static Firecracker guest runtime is not executable: $GUEST_RUNTIME" >&2
  exit 1
fi
for command in basename chmod cpio dirname find install mkdir mktemp mv rm sort; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done

mkdir -p "$(dirname "$OUTPUT")"
OUTPUT="$(cd "$(dirname "$OUTPUT")" && pwd -P)/$(basename "$OUTPUT")"
WORK_DIR="$(mktemp -d)"
OUTPUT_TEMP="$(mktemp "${OUTPUT}.tmp.XXXXXX")"

cleanup() {
  rm -rf "$WORK_DIR"
  rm -f "$OUTPUT_TEMP"
}
trap cleanup EXIT

install -m 0755 "$GUEST_RUNTIME" "$WORK_DIR/init"

# Firecracker loads an uncompressed newc archive directly into guest memory.
# Keeping PID 1 as the only payload minimizes both load and exec latency.
# https://github.com/firecracker-microvm/firecracker/blob/main/docs/initrd.md#custom
(
  cd "$WORK_DIR"
  find . -print0 | sort -z | cpio --null --create --format=newc --quiet >"$OUTPUT_TEMP"
)
chmod 0644 "$OUTPUT_TEMP"
mv "$OUTPUT_TEMP" "$OUTPUT"
OUTPUT_TEMP=""
printf '%s\n' "$OUTPUT"
