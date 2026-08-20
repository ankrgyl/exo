#!/usr/bin/env bash
set -euo pipefail

action="${1:-}"
instance="${EXO_FIRECRACKER_LIMA_INSTANCE:-exo-firecracker}"
state_root="${EXO_FIRECRACKER_STATE_ROOT:-/var/lib/exo/firecracker/state}"

case "$action" in
  restart | clean) ;;
  *)
    echo "usage: $0 {restart|clean}" >&2
    exit 2
    ;;
esac

case "$state_root" in
  /var/lib/exo/firecracker/*) ;;
  *)
    echo "refusing to clean unexpected Firecracker state root: $state_root" >&2
    exit 2
    ;;
esac

case "$state_root/" in
  *'/../'* | *'/./'* | *'//'*)
    echo "refusing to clean non-canonical Firecracker state root: $state_root" >&2
    exit 2
    ;;
esac

if ! limactl list --format '{{.Name}}' |
  awk -v instance="$instance" '$0 == instance { found = 1 } END { exit !found }'; then
  echo "Lima instance does not exist: $instance" >&2
  exit 1
fi

limactl stop --tty=false "$instance"
limactl start --tty=false "$instance"

if [[ "$action" == clean ]]; then
  limactl shell "$instance" -- sudo -n rm -rf -- \
    "$state_root/cows" \
    "$state_root/jailer" \
    "$state_root/leases" \
    "$state_root/manifests" \
    "$state_root/slots" \
    "$state_root/snapshots" \
    "$state_root/workspaces"
fi

limactl list
