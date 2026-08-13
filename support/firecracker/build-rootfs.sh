#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 --image <oci-image> --output <rootfs.ext4> [--size-gib <size>]" >&2
}

IMAGE=""
OUTPUT=""
SIZE_GIB=8

while [[ $# -gt 0 ]]; do
  case "$1" in
    --image)
      IMAGE="$2"
      shift 2
      ;;
    --output)
      OUTPUT="$2"
      shift 2
      ;;
    --size-gib)
      SIZE_GIB="$2"
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

if [[ -z "$IMAGE" || -z "$OUTPUT" ]]; then
  usage
  exit 2
fi

if [[ "$EUID" -ne 0 ]]; then
  echo "Root is required so the image records guest UID 10001 ownership" >&2
  exit 1
fi

for command in basename chmod chown dirname docker install ln mkdir mkfs.ext4 mktemp realpath rm tar truncate; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
mkdir -p "$(dirname "$OUTPUT")"
OUTPUT="$(cd "$(dirname "$OUTPUT")" && pwd -P)/$(basename "$OUTPUT")"
if [[ -e "$OUTPUT" || -L "$OUTPUT" ]]; then
  echo "Output already exists; refusing to overwrite it: $OUTPUT" >&2
  exit 1
fi
WORK_DIR="$(mktemp -d)"
CONTAINER_ID=""
OUTPUT_TEMP=""

cleanup() {
  if [[ -n "$CONTAINER_ID" ]]; then
    docker rm "$CONTAINER_ID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$OUTPUT_TEMP" && -e "$OUTPUT_TEMP" ]]; then
    rm -f "$OUTPUT_TEMP"
  fi
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

# Firecracker block devices are host files containing a guest-supported filesystem.
# See https://github.com/firecracker-microvm/firecracker/blob/main/docs/rootfs-and-kernel-setup.md#creating-a-linux-rootfs-image
CONTAINER_ID="$(docker create "$IMAGE")"
mkdir "$WORK_DIR/rootfs"
docker export "$CONTAINER_ID" | tar -C "$WORK_DIR/rootfs" -xf -
docker rm "$CONTAINER_ID" >/dev/null
CONTAINER_ID=""

assert_inside_rootfs() {
  local candidate="$1"
  local resolved
  resolved="$(realpath -m "$candidate")"
  case "$resolved" in
    "$WORK_DIR/rootfs" | "$WORK_DIR/rootfs"/*) ;;
    *)
      echo "OCI image path escapes its root filesystem: $candidate -> $resolved" >&2
      exit 1
      ;;
  esac
}

for required in bin/sh usr/bin/cat usr/bin/chown usr/bin/mkdir usr/bin/mount usr/bin/mountpoint usr/bin/python3 usr/bin/rm usr/bin/setpriv usr/sbin/ip; do
  assert_inside_rootfs "$WORK_DIR/rootfs/$required"
  if [[ ! -x "$WORK_DIR/rootfs/$required" ]]; then
    echo "OCI image must contain /$required" >&2
    exit 1
  fi
done

assert_inside_rootfs "$WORK_DIR/rootfs/runtime"
assert_inside_rootfs "$WORK_DIR/rootfs/home/exo/workspace"
mkdir -p "$WORK_DIR/rootfs/runtime" "$WORK_DIR/rootfs/home/exo/workspace"
install -m 0755 "$SCRIPT_DIR/exo-firecracker-init" "$WORK_DIR/rootfs/runtime/exo-firecracker-init"
install -m 0755 "$SCRIPT_DIR/exo-firecracker-agent.py" "$WORK_DIR/rootfs/runtime/exo-firecracker-agent.py"
chown -R 10001:10001 "$WORK_DIR/rootfs/home/exo"

OUTPUT_TEMP="$(mktemp "${OUTPUT}.tmp.XXXXXX")"
truncate -s "${SIZE_GIB}G" "$OUTPUT_TEMP"
mkfs.ext4 -q -F -d "$WORK_DIR/rootfs" "$OUTPUT_TEMP"
chmod 0600 "$OUTPUT_TEMP"
ln "$OUTPUT_TEMP" "$OUTPUT"
rm "$OUTPUT_TEMP"
OUTPUT_TEMP=""
echo "$OUTPUT"
