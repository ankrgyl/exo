#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 --image <oci-image> --output <rootfs.ext4> [--guest-runtime <path>] [--size-gib <size>]" >&2
}

IMAGE=""
OUTPUT=""
SIZE_GIB=8
GUEST_RUNTIME="/var/lib/exo/firecracker/exo-firecracker-guest"

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
    --guest-runtime)
      GUEST_RUNTIME="$2"
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

if [[ ! -x "$GUEST_RUNTIME" ]]; then
  echo "Static Firecracker guest runtime is not executable: $GUEST_RUNTIME" >&2
  exit 1
fi

if [[ "$EUID" -ne 0 ]]; then
  echo "Root is required so the image records guest UID 10001 ownership" >&2
  exit 1
fi

for command in basename chmod chown dirname docker install ln mkdir mkfs.ext4 mktemp readlink rm tar truncate; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done

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

resolve_inside_rootfs() {
  local root="$WORK_DIR/rootfs"
  local candidate="$1"
  case "$candidate" in
    "$root" | "$root"/*) ;;
    *)
      echo "OCI image path is outside its root filesystem: $candidate" >&2
      return 1
      ;;
  esac

  # Absolute symlink targets in an OCI rootfs are relative to the guest root.
  # Resolve components ourselves so merged-/usr links such as /usr/sbin/ip ->
  # /usr/bin/ip cannot escape into the build host, without executing image code.
  # https://github.com/opencontainers/image-spec/blob/main/layer.md
  local remainder="${candidate#"$root"}"
  remainder="${remainder#/}"
  local resolved="$root"
  local symlinks=0

  while [[ -n "$remainder" ]]; do
    local component="${remainder%%/*}"
    if [[ "$remainder" == */* ]]; then
      remainder="${remainder#*/}"
    else
      remainder=""
    fi

    case "$component" in
      "" | ".") ;;
      "..")
        if [[ "$resolved" != "$root" ]]; then
          resolved="${resolved%/*}"
        fi
        ;;
      *)
        local next="$resolved/$component"
        if [[ -L "$next" ]]; then
          symlinks=$((symlinks + 1))
          if ((symlinks > 40)); then
            echo "Too many OCI image symlinks while resolving: $candidate" >&2
            return 1
          fi
          local target
          target="$(readlink "$next")"
          if [[ "$target" == /* ]]; then
            resolved="$root"
            target="${target#/}"
          fi
          if [[ -n "$remainder" ]]; then
            remainder="$target/$remainder"
          else
            remainder="$target"
          fi
        else
          resolved="$next"
        fi
        ;;
    esac
  done

  case "$resolved" in
    "$root" | "$root"/*) printf '%s\n' "$resolved" ;;
    *)
      echo "OCI image path escapes its root filesystem: $candidate -> $resolved" >&2
      return 1
      ;;
  esac
}

runtime_dir="$(resolve_inside_rootfs "$WORK_DIR/rootfs/runtime")"
workspace_dir="$(resolve_inside_rootfs "$WORK_DIR/rootfs/home/exo/workspace")"
mkdir -p "$runtime_dir" "$workspace_dir"
install -m 0755 "$GUEST_RUNTIME" "$runtime_dir/exo-firecracker-guest"
chown -R 10001:10001 "$(resolve_inside_rootfs "$WORK_DIR/rootfs/home/exo")"

OUTPUT_TEMP="$(mktemp "${OUTPUT}.tmp.XXXXXX")"
truncate -s "${SIZE_GIB}G" "$OUTPUT_TEMP"
mkfs.ext4 -q -F -d "$WORK_DIR/rootfs" "$OUTPUT_TEMP"
chmod 0600 "$OUTPUT_TEMP"
ln "$OUTPUT_TEMP" "$OUTPUT"
rm "$OUTPUT_TEMP"
OUTPUT_TEMP=""
echo "$OUTPUT"
