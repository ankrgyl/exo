#!/usr/bin/env bash

# Functions prefixed with _exo_macos_ are internal to this sourced script.
_exo_macos_keychain_item_has_partitions() {
  local keychain_path="$1"
  local account="$2"
  local partition_list="$3"

  security dump-keychain -a "$keychain_path" 2>&1 |
    awk -v account="$account" -v partitions="$partition_list" '
      BEGIN { expected = split(partitions, partition, ",") }
      /^keychain: / { has_account = 0 }
      index($0, "\"acct\"<blob>=\"" account "\"") { has_account = 1 }
      has_account {
        for (i = 1; i <= expected; i++) {
          if (index($0, partition[i])) {
            found[i] = 1
          }
        }
      }
      END {
        for (i = 1; i <= expected; i++) {
          if (!found[i]) {
            exit 1
          }
        }
      }
    '
}

# Resolve the account of an existing master-key item. New stores use their
# persisted store ID; stores awaiting migration still use the legacy shared
# account until Exo copies the key into the namespaced item.
_exo_macos_keychain_account_for_existing_master_key() {
  local root_dir="$1"
  local keychain_path="$2"
  local service="$3"
  local legacy_account=".exo/exoharness"
  local identity_file="$root_dir/.exo/exoharness/metadata/secret-store.json"

  if [[ ! -f "$identity_file" ]]; then
    printf '%s' "$legacy_account"
    return 0
  fi
  if ! command -v node >/dev/null 2>&1; then
    echo "error: node is required to read the Exo secret-store identity" >&2
    return 1
  fi

  local store_id
  if ! store_id="$(node -e '
    const fs = require("node:fs");
    const identity = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    if (typeof identity.id !== "string" || identity.id.length === 0) {
      throw new Error("invalid Exo secret-store identity");
    }
    process.stdout.write(identity.id);
  ' "$identity_file")"; then
    echo "error: could not read the Exo secret-store identity" >&2
    return 1
  fi

  local namespaced_account="exo-secret-store:$store_id"
  local find_status=0
  security find-generic-password -a "$namespaced_account" -s "$service" \
    "$keychain_path" >/dev/null 2>&1 || find_status=$?
  case "$find_status" in
    0) printf '%s' "$namespaced_account" ;;
    # errSecItemNotFound (-25300) is truncated to an 8-bit shell status.
    44) printf '%s' "$legacy_account" ;;
    *)
      echo "error: could not query the namespaced Exo master key" >&2
      return "$find_status"
      ;;
  esac
}

_exo_macos_unlock_keychain() {
  local keychain_path="$1"
  echo "The macOS login keychain must be unlocked before Exo can use credentials over SSH."
  echo "Enter the macOS login keychain password when prompted."
  if ! security unlock-keychain "$keychain_path"; then
    echo "error: could not unlock the macOS login keychain" >&2
    return 1
  fi
}

_exo_macos_ensure_keychain_access() {
  local exo_binary="$1"
  local keychain_path="$2"

  if "$exo_binary" secret check >/dev/null 2>&1; then
    return 0
  fi

  if ! _exo_macos_unlock_keychain "$keychain_path"; then
    return 1
  fi
  if ! "$exo_binary" secret check >/dev/null 2>&1; then
    echo "error: Exo still cannot access the macOS login keychain" >&2
    return 1
  fi
}

# Over SSH, unlock the login keychain and authorize the current Exo CLI and
# scheduler builds to read an existing master key. Stable signing preserves
# their application identity, while the partition ACL permits headless access
# for each build's CDHash.
exo_macos_prepare_keychain_for_ssh() {
  local root_dir="$1"
  shift
  local exo_binary="${1:-}"

  if [[ "$(uname -s)" != "Darwin" ]] ||
    [[ -z "${SSH_CONNECTION:-}" && -z "${SSH_TTY:-}" ]] ||
    [[ "${EXO_SECRET_BACKEND:-apple-keychain}" != "apple-keychain" ]]; then
    return 0
  fi

  if [[ ! -t 0 ]]; then
    echo "error: macOS Keychain setup over SSH requires an interactive terminal" >&2
    return 1
  fi
  if ! command -v security >/dev/null 2>&1; then
    echo "error: the macOS security command is required for Keychain setup" >&2
    return 1
  fi
  if ! command -v codesign >/dev/null 2>&1; then
    echo "error: codesign is required to authorize Exo development builds" >&2
    return 1
  fi

  local keychain_path
  keychain_path="$(security default-keychain -d user |
    sed 's/^[[:space:]]*"//; s/"[[:space:]]*$//')"
  if [[ -z "$keychain_path" ]]; then
    echo "error: could not determine the default user keychain" >&2
    return 1
  fi

  local service="exo-exoharness-master-key"
  local account
  if ! account="$(_exo_macos_keychain_account_for_existing_master_key \
    "$root_dir" "$keychain_path" "$service")"; then
    return 1
  fi

  local find_status=0
  security find-generic-password -a "$account" -s "$service" \
    "$keychain_path" >/dev/null 2>&1 || find_status=$?
  if [[ "$find_status" -eq 44 ]]; then
    # A fresh store has no master-key item to authorize yet. Unlock the
    # Keychain so the caller can create it by storing the first secret, then
    # authorize the new item when this helper is invoked a second time.
    if ! _exo_macos_unlock_keychain "$keychain_path"; then
      return 1
    fi
    return 0
  elif [[ "$find_status" -ne 0 ]]; then
    echo "error: could not query the Exo master key" >&2
    return "$find_status"
  fi

  local partition_list=""
  local binary cdhash partition
  for binary in "$@"; do
    if [[ ! -e "$binary" ]]; then
      continue
    fi
    if [[ ! -x "$binary" ]]; then
      echo "error: Exo development binary is not executable: $binary" >&2
      return 1
    fi
    cdhash="$(codesign -dvvv "$binary" 2>&1 |
      sed -n 's/^CDHash=//p' | head -n 1)"
    if [[ ! "$cdhash" =~ ^[[:xdigit:]]{40}$ ]]; then
      echo "error: could not determine the development build CDHash for $binary" >&2
      return 1
    fi
    partition="cdhash:$cdhash"
    if [[ -z "$partition_list" ]]; then
      partition_list="$partition"
    elif [[ ",$partition_list," != *",$partition,"* ]]; then
      partition_list="$partition_list,$partition"
    fi
  done
  if [[ -z "$partition_list" ]]; then
    return 0
  fi

  if _exo_macos_keychain_item_has_partitions "$keychain_path" "$account" "$partition_list"; then
    if ! _exo_macos_ensure_keychain_access "$exo_binary" "$keychain_path"; then
      return 1
    fi
    return 0
  fi

  echo "The current Exo development binaries need access to the Keychain master key."
  echo "Apple may label its password prompt below as '(deprecated)'."
  echo "That label refers to the security command interface, not your password."
  local security_output
  if ! security_output="$(security set-generic-password-partition-list \
    -a "$account" -s "$service" -S "$partition_list" \
    "$keychain_path" 2>&1 >/dev/null)"; then
    echo
    if [[ -n "$security_output" ]]; then
      echo "$security_output" >&2
    fi
    echo "error: could not authorize the Exo development binaries for Keychain access" >&2
    return 1
  fi
  echo
  if ! _exo_macos_keychain_item_has_partitions "$keychain_path" "$account" "$partition_list"; then
    echo "error: macOS Keychain did not retain access for every Exo binary" >&2
    return 1
  fi

  _exo_macos_ensure_keychain_access "$exo_binary" "$keychain_path"
}
