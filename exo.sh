#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# setup.sh installs node/pnpm/rust through mise; make its shims available even
# in shells where mise was never activated (the harness runner spawns node).
if [[ -x "$HOME/.local/bin/mise" ]] || command -v mise >/dev/null 2>&1; then
  export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"
fi
if [[ -f "$HOME/.cargo/env" ]]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

EXO_BIN="${EXO_BIN:-$ROOT_DIR/target/debug/exo}"
SCHEDULER_BIN="${EXO_SCHEDULER_BIN:-$ROOT_DIR/target/debug/exo-scheduler-runner}"
ENV_FILE="${EXO_ENV_FILE:-$ROOT_DIR/.env}"
MODEL="${EXO_MODEL:-gpt-5.6-terra}"
AGENT="${EXO_AGENT:-exo-agent}"
AGENT_NAME="${EXO_AGENT_NAME:-Exo Agent}"
CONVERSATION="${EXO_CONVERSATION:-dev}"
CONVERSATION_NAME="${EXO_CONVERSATION_NAME:-Dev}"
MODULE="${EXO_MODULE:-examples/exo/harness.ts}"
HARNESS="exo"
LOCAL_PROMPT_FILE="${EXO_LOCAL_PROMPT_FILE:-$ROOT_DIR/.exo/exo-profile.md}"
SANDBOX_IMAGE="${EXO_SANDBOX_IMAGE:-ubuntu:24.04}"
SANDBOX_PROVIDER="${EXO_SANDBOX_PROVIDER:-}"
SANDBOX_BACKEND="${EXO_SANDBOX_BACKEND:-}"
SELF_REPO_MOUNT_PATH="${EXO_REPO:-/workspace/exo}"
SELF_MAP_PATH="$SELF_REPO_MOUNT_PATH/examples/exo/SELF.md"
AGENT_CLI_MOUNT_ROOT="${EXO_AGENT_CLI_ROOT:-}"
AGENT_CLI_MOUNT_PATH="${EXO_AGENT_CLI_MOUNT:-/agent-cli}"
NETWORKING="${EXO_NETWORKING:-enabled}"
SHELL_PROGRAM="${EXO_SHELL_PROGRAM:-/bin/bash}"
SANDBOX_SCOPE="${EXO_SANDBOX_SCOPE:-}"
SCHEDULER_INTERVAL_SECONDS="${EXO_SCHEDULER_INTERVAL_SECONDS:-10}"
COMMAND="repl"
USE_SANDBOX=true
PULL_SANDBOX=false
START_SCHEDULER="${EXO_START_SCHEDULER:-true}"
START_ADAPTERS="${EXO_START_ADAPTERS:-true}"
ADAPTER_LIMIT="${EXO_ADAPTER_LIMIT:-50}"
CONTROL=false
SETUP_PROFILE=false
SKIP_BUILD="${EXO_SKIP_BUILD:-false}"
TEMPLATE="${EXO_TEMPLATE:-canonical}"
SANDBOX_PROVIDER_EXPLICIT=false
SANDBOX_BACKEND_EXPLICIT=false
declare -a CONTROL_PIDS=()
SETUP_ADAPTER="${EXO_SETUP_ADAPTER:-}"
declare -a SETUP_ADAPTERS=()
if [[ -n "$SETUP_ADAPTER" ]]; then
  SETUP_ADAPTERS+=("$SETUP_ADAPTER")
fi
INITIAL_PROMPT_FILE="${EXO_INITIAL_PROMPT_FILE:-}"
UPSTREAM_MODEL="${EXO_UPSTREAM_MODEL:-}"
SECRET_NAME=""
SECRET_ENV=""
MODEL_BASE_URL=""
USER_NAME="${EXO_USER_NAME:-}"
export EXO_LOCAL_PROMPT_FILE="$LOCAL_PROMPT_FILE"

usage() {
  cat <<'EOF'
Usage:
  ./exo.sh [options]
  ./exo.sh list
  ./exo.sh delall all
  ./exo.sh fresh
  ./exo.sh stop-all
  ./exo.sh build
  ./exo.sh register-model
  ./exo.sh write-profile
  ./exo.sh setup-profile
  ./exo.sh setup-sandbox

Default behavior starts the canonical stack: it creates or reuses an Exo
agent and conversation with a Docker sandbox, repo self-map mount, ExoChat
setup, guardian config, and control logs, starts the local scheduler and
adapter loops, then starts a REPL. It reads .env by default if present.
Choose a different template with --template.

Subcommands:
  list             List agents and conversations
  delall all       Delete all agents and conversations
  fresh            Rebuild, delete all state, and start a clean REPL
  stop-all         Stop the scheduler and adapter runners, preserving .exo state
  build            Install JS dependencies and build the exo CLI and scheduler
  register-model   Store an API-key secret and register a model binding; uses
                   --model, --upstream-model, --secret-name, --secret-env, and
                   optionally --base-url
  write-profile    Write the local profile prompt non-interactively; uses
                   --user-name and --local-prompt-file
  setup-profile    Prompt interactively and write the local profile prompt
  setup-sandbox    Pull the sandbox image
  setup-agent      Create the agent and conversation (and pull the sandbox
                   image) without starting anything

Options:
  --model <model>              Model binding name (default: gpt-5.6-terra)
  --upstream-model <model>     Upstream model id for register-model (default: --model)
  --secret-name <name>         Secret name for register-model (e.g. openai)
  --secret-env <env-var>       Environment variable holding the API key for register-model
  --base-url <url>             Optional API base URL for register-model
  --user-name <name>           User name for write-profile (default: none)
  --agent <slug>               Agent slug (default: exo-agent)
  --conversation <slug>        Conversation slug (default: dev)
  --convo <slug>               Alias for --conversation
  --agent-name <name>          Agent display name (default: Exo Agent)
  --conversation-name <name>   Conversation display name (default: Dev)
  --module <path>              Exo TypeScript harness module
  --template <name>            Launch template (default: canonical):
                                 canonical  Docker sandbox, repo self-map mount, ExoChat
                                            setup, control logs, and guardian config
                                 dev        Same as canonical but with IRC+Discord
                                            instead of ExoChat
                                 minimal    No Docker defaults, adapter setup prompts,
                                            control console, or guardian config
  --sandbox-image <image>      Sandbox image (default: ubuntu:24.04)
  --sandbox-provider <provider>
                                Sandbox provider: daytona, apple-container, docker, or local-process
  --sandbox-backend <backend>   Local sandbox backend: apple-container, docker, or local-process.
                                Defaults to --sandbox-provider when that provider is local.
  --self-repo-mount <path>      Sandbox path for this repo (default: /workspace/exo)
  --agent-cli-mount <host-dir>  Bind-mount this host directory read-write into the
                                sandbox for the agent-cli adapter (default: none)
  --agent-cli-mount-path <path> Sandbox path for the agent-cli mount (default: /agent-cli)
  --networking <mode>          enabled or disabled (default: enabled)
  --shell-program <path>       Shell in the sandbox (default: /bin/bash)
  --sandbox-scope <scope>      agent or conversation (default: Exo agent)
  --scheduler-interval <secs>  Scheduler polling interval (default: 10)
  --no-scheduler               Do not start the local scheduled task runner
  --scheduler                  Start the local scheduled task runner
  --no-adapters                Do not start the local adapter runner
  --adapters                   Start the local adapter runner
  --adapter-limit <n>          Max adapters supervised by the runner (default: 50)
  --control                    Show live scheduler and adapter logs beside the REPL
  --setup-profile              Prompt once and write the ignored local profile prompt
  --local-prompt-file <path>    Local profile prompt path (default: .exo/exo-profile.md)
  --setup <adapter>            Send adapters/<adapter>/setup-prompt.md.
                                May be passed more than once.
  --setup-all                  Equivalent to --setup exochat --setup signal
                               --setup whatsapp --setup irc --setup discord.
                                For exochat, print a browser URL; for whatsapp/signal,
                                print pairing QR and pause.
  --initial-prompt-file <path> Send this file as the first message before REPL
  --pull-sandbox               Pull the sandbox image before starting
  --skip-build                 Do not build the exo CLI before starting; requires
                               --exo-bin to already exist
  --no-sandbox                 Do not require or configure sandbox shell support
  --env-file <path>            Env file to read if present (default: .env)
  --exo-bin <path>             exo binary path (default: ./target/debug/exo)
  --scheduler-bin <path>       Scheduler runner path (default: ./target/debug/exo-scheduler-runner)
  --help                       Show this help

Environment overrides:
  EXO_MODEL, EXO_AGENT, EXO_CONVERSATION, EXO_AGENT_NAME,
  EXO_CONVERSATION_NAME, EXO_MODULE, EXO_SANDBOX_IMAGE,
  EXO_SANDBOX_PROVIDER, EXO_SANDBOX_BACKEND, EXO_NETWORKING,
  EXO_SHELL_PROGRAM, EXO_SANDBOX_SCOPE, EXO_ENV_FILE, EXO_LOCAL_PROMPT_FILE,
  EXO_BIN, EXO_START_SCHEDULER, EXO_START_ADAPTERS, EXO_REPO,
  EXO_AGENT_CLI_ROOT, EXO_AGENT_CLI_MOUNT,
  EXO_SCHEDULER_BIN, EXO_SCHEDULER_INTERVAL_SECONDS, EXO_ADAPTER_LIMIT,
  EXO_SETUP_ADAPTER, EXO_INITIAL_PROMPT_FILE, EXO_TEMPLATE,
  EXO_SKIP_BUILD, EXO_UPSTREAM_MODEL, EXO_USER_NAME
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

terminate_process_tree() {
  local pid="$1"
  kill_process_tree TERM "$pid"
  sleep 1
  kill_process_tree KILL "$pid"
}

kill_process_tree() {
  local signal="$1"
  local pid="$2"
  local child
  [[ "$pid" =~ ^[0-9]+$ ]] || return
  if command -v pgrep >/dev/null 2>&1; then
    while IFS= read -r child; do
      [[ -n "$child" ]] || continue
      kill_process_tree "$signal" "$child"
    done < <(pgrep -P "$pid" 2>/dev/null || true)
  fi
  kill "-$signal" "$pid" >/dev/null 2>&1 || true
}

ensure_exo_bin() {
  if [[ -x "$EXO_BIN" ]]; then
    return
  fi
  if [[ "$SKIP_BUILD" == true ]]; then
    die "exo binary is not executable: $EXO_BIN (--skip-build was set)"
  fi
  if [[ "$EXO_BIN" != "$ROOT_DIR/target/debug/exo" ]]; then
    die "exo binary is not executable: $EXO_BIN"
  fi
  build_exo
}

ensure_scheduler_bin() {
  if [[ -x "$SCHEDULER_BIN" ]] && ! scheduler_source_newer_than "$SCHEDULER_BIN"; then
    return
  fi
  if [[ "$SCHEDULER_BIN" != "$ROOT_DIR/target/debug/exo-scheduler-runner" ]]; then
    die "scheduler runner is not executable: $SCHEDULER_BIN"
  fi
  build_exo_scheduler
}

ensure_signal_cli() {
  if ! setup_requested "signal"; then
    return
  fi
  if ! command -v signal-cli >/dev/null 2>&1; then
    die "signal-cli is required for --setup signal. Install it first, for example: brew install signal-cli"
  fi

  local signal_cli_path
  signal_cli_path="$(command -v signal-cli)"
  if command -v file >/dev/null 2>&1 && file "$signal_cli_path" | grep -q "Mach-O"; then
    echo "Warning: $signal_cli_path appears to be a native signal-cli executable."
    echo "If outbound sends fail with NETWORK_FAILURE, use the JVM signal-cli distribution instead."
  fi
}

setup_requested() {
  local adapter="$1"
  local requested
  if [[ "${#SETUP_ADAPTERS[@]}" -eq 0 ]]; then
    return 1
  fi
  for requested in "${SETUP_ADAPTERS[@]}"; do
    if [[ "$requested" == "$adapter" ]]; then
      return 0
    fi
  done
  return 1
}

add_setup_adapter() {
  local adapter="$1"
  [[ -n "$adapter" ]] || die "--setup requires an adapter name"
  if [[ ! "$adapter" =~ ^[a-zA-Z0-9_-]+$ ]]; then
    die "--setup must be an adapter name, not a path: $adapter"
  fi
  if setup_requested "$adapter"; then
    return
  fi
  SETUP_ADAPTERS+=("$adapter")
}

apply_template_defaults() {
  if [[ "$TEMPLATE" == "minimal" ]]; then
    return
  fi

  USE_SANDBOX=true
  PULL_SANDBOX=true
  START_SCHEDULER=true
  START_ADAPTERS=true
  CONTROL=true
  if [[ "$SANDBOX_PROVIDER_EXPLICIT" != true ]]; then
    SANDBOX_PROVIDER="docker"
  fi
  if [[ "$SANDBOX_BACKEND_EXPLICIT" != true ]]; then
    SANDBOX_BACKEND="docker"
  fi
  case "$TEMPLATE" in
    canonical)
      add_setup_adapter "exochat"
      ;;
    dev)
      add_setup_adapter "irc"
      add_setup_adapter "discord"
      ;;
    *)
      die "--template must be canonical, dev, or minimal"
      ;;
  esac
}

configure_guardian_for_current_launch() {
  if [[ "$TEMPLATE" == "minimal" ]]; then
    return
  fi

  "$ROOT_DIR/examples/exo/scripts/exo-service-guardian" configure \
    --env-file "$ENV_FILE" \
    --exo-bin "$EXO_BIN" \
    --scheduler-bin "$SCHEDULER_BIN" \
    --sandbox-backend "$SANDBOX_BACKEND" \
    --scheduler-interval "$SCHEDULER_INTERVAL_SECONDS" \
    --adapter-limit "$ADAPTER_LIMIT" >/dev/null
  echo "Configured guardian for Docker-backed Exo services."
}

build_exo() {
  echo "Building exo binary..."
  (cd "$ROOT_DIR" && CARGO_TARGET_DIR=target cargo build -p exo --ignore-rust-version)
}

build_exo_scheduler() {
  echo "Building Exo scheduler runner..."
  (cd "$ROOT_DIR" && CARGO_TARGET_DIR=target cargo build \
    --manifest-path examples/exo/scheduler-runner/Cargo.toml \
    --ignore-rust-version)
}

build_all() {
  echo "Installing JS dependencies..."
  (cd "$ROOT_DIR" && pnpm install)
  build_exo
  build_exo_scheduler
}

register_model() {
  [[ -n "$SECRET_NAME" ]] || die "register-model requires --secret-name"
  [[ -n "$SECRET_ENV" ]] || die "register-model requires --secret-env"
  ensure_exo_bin
  local upstream="${UPSTREAM_MODEL:-$MODEL}"
  echo "Storing secret $SECRET_NAME from \$$SECRET_ENV..."
  exo secret set "$SECRET_NAME" --env "$SECRET_ENV"
  echo "Registering model $MODEL -> $upstream..."
  local args=(model register "$MODEL" --model "$upstream" --secret "$SECRET_NAME")
  if [[ -n "$MODEL_BASE_URL" ]]; then
    args+=(--base-url "$MODEL_BASE_URL")
  fi
  exo "${args[@]}"
}

write_local_profile() {
  mkdir -p "$(dirname "$LOCAL_PROMPT_FILE")"
  {
    echo "# Local Exo Profile"
    echo
    echo "This file is local to this machine and should not be committed."
    if [[ -n "$USER_NAME" ]]; then
      echo
      echo "The user's name is $USER_NAME."
    fi
  } >"$LOCAL_PROMPT_FILE"
  chmod 600 "$LOCAL_PROMPT_FILE"
  echo "Wrote local Exo profile: $LOCAL_PROMPT_FILE"
}

scheduler_source_newer_than() {
  local target="$1"
  local path
  for path in \
    "$ROOT_DIR/examples/exo/scheduler-runner/Cargo.toml" \
    "$ROOT_DIR/examples/exo/scheduler-runner/src"/*.rs; do
    if [[ -e "$path" && "$path" -nt "$target" ]]; then
      return 0
    fi
  done
  return 1
}

effective_sandbox_backend() {
  if [[ -n "$SANDBOX_BACKEND" ]]; then
    printf '%s\n' "$SANDBOX_BACKEND"
    return
  fi
  case "$SANDBOX_PROVIDER" in
    apple-container|docker|local-process)
      printf '%s\n' "$SANDBOX_PROVIDER"
      ;;
  esac
}

append_exo_global_args() {
  local backend
  EXO_GLOBAL_ARGS=(--env-file-if-exists "$ENV_FILE")
  backend="$(effective_sandbox_backend)"
  if [[ -n "$backend" ]]; then
    EXO_GLOBAL_ARGS+=(--sandbox-backend "$backend")
  fi
}

exo() {
  EXO_GLOBAL_ARGS=()
  append_exo_global_args
  "$EXO_BIN" "${EXO_GLOBAL_ARGS[@]}" "$@"
}

scheduler_pid_file() {
  echo "$ROOT_DIR/.exo/exo-scheduler.pid"
}

scheduler_lock_file() {
  echo "$ROOT_DIR/.exo/exo-scheduler.lock"
}

scheduler_log_file() {
  echo "$ROOT_DIR/.exo/exo-scheduler.log"
}

repl_restart_file() {
  echo "$ROOT_DIR/.exo/exo-control.restart"
}

adapters_pid_file() {
  echo "$ROOT_DIR/.exo/exo-adapters.pid"
}

adapters_lock_file() {
  echo "$ROOT_DIR/.exo/exo-adapters.lock"
}

adapters_restart_file() {
  echo "$ROOT_DIR/.exo/exo-adapters.restart"
}

reboot_notice_file() {
  echo "$ROOT_DIR/.exo/exo-reboot-notice.json"
}

adapters_log_file() {
  echo "$ROOT_DIR/.exo/exo-adapters.log"
}

scheduler_process_running() {
  local pid_file pid command_line
  pid_file="$(scheduler_pid_file)"
  [[ -f "$pid_file" ]] || return 1
  pid="$(<"$pid_file")"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" >/dev/null 2>&1 || return 1
  if [[ "$SCHEDULER_BIN" -nt "$pid_file" ]]; then
    echo "Restarting scheduler because $SCHEDULER_BIN is newer..."
    terminate_process_tree "$pid"
    return 1
  fi
  command_line="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  [[ "$command_line" == *"exo-scheduler-runner"*"run --watch"* ]]
}

adapters_process_running() {
  local pid_file pid command_line
  pid_file="$(adapters_pid_file)"
  [[ -f "$pid_file" ]] || return 1
  pid="$(<"$pid_file")"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" >/dev/null 2>&1 || return 1
  if [[ "$EXO_BIN" -nt "$pid_file" ]]; then
    echo "Restarting adapter runner because $EXO_BIN is newer..."
    terminate_process_tree "$pid"
    return 1
  fi
  if adapter_source_newer_than "$pid_file"; then
    echo "Restarting adapter runner because adapter code changed..."
    terminate_process_tree "$pid"
    return 1
  fi
  command_line="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  [[ "$command_line" == *"adapters run"* ]]
}

adapter_source_newer_than() {
  local target="$1"
  local path
  for path in \
    "$ROOT_DIR/examples/exo/adapters/protocol.ts" \
    "$ROOT_DIR/examples/exo/adapters"/*/worker.ts; do
    if [[ -e "$path" && "$path" -nt "$target" ]]; then
      return 0
    fi
  done
  return 1
}

ensure_scheduler() {
  if [[ "$START_SCHEDULER" != true ]]; then
    return
  fi
  ensure_scheduler_bin
  if scheduler_process_running; then
    return
  fi

  mkdir -p "$ROOT_DIR/.exo"
  local pid_file log_file
  pid_file="$(scheduler_pid_file)"
  log_file="$(scheduler_log_file)"
  echo "Starting scheduler loop..."
  rm -f "$(scheduler_lock_file)"
  local scheduler_args=(--env-file-if-exists "$ENV_FILE")
  local backend
  backend="$(effective_sandbox_backend)"
  if [[ -n "$backend" ]]; then
    scheduler_args+=(--sandbox-backend "$backend")
  fi
  nohup "$SCHEDULER_BIN" "${scheduler_args[@]}" run --watch \
    --interval-seconds "$SCHEDULER_INTERVAL_SECONDS" >>"$log_file" 2>&1 &
  echo "$!" >"$pid_file"
  echo "Scheduler log: $log_file"
}

ensure_adapters() {
  if [[ "$START_ADAPTERS" != true ]]; then
    return
  fi
  if adapters_process_running; then
    return
  fi

  mkdir -p "$ROOT_DIR/.exo"
  local pid_file log_file
  pid_file="$(adapters_pid_file)"
  log_file="$(adapters_log_file)"
  echo "Starting adapter runner..."
  EXO_GLOBAL_ARGS=()
  append_exo_global_args
  nohup "$EXO_BIN" "${EXO_GLOBAL_ARGS[@]}" --harness "$HARNESS" \
    adapters run \
      --limit "$ADAPTER_LIMIT" \
      --lock-file "$(adapters_lock_file)" \
      --drain-marker "$(adapters_restart_file)" \
      --reboot-notice "$(reboot_notice_file)" \
    >>"$log_file" 2>&1 &
  echo "$!" >"$pid_file"
  echo "Adapter log: $log_file"
}

container_image_exists() {
  if command -v docker >/dev/null 2>&1; then
    docker image inspect "$SANDBOX_IMAGE" >/dev/null 2>&1
    return
  fi
  if command -v podman >/dev/null 2>&1; then
    podman image exists "$SANDBOX_IMAGE" >/dev/null 2>&1
    return
  fi
  return 2
}

container_pull_image() {
  if command -v docker >/dev/null 2>&1; then
    docker pull "$SANDBOX_IMAGE"
    return
  fi
  if command -v podman >/dev/null 2>&1; then
    podman pull "$SANDBOX_IMAGE"
    return
  fi
  die "docker or podman is required to pre-pull sandbox images"
}

ensure_sandbox_image() {
  local status=0
  container_image_exists || status=$?
  case "$status" in
    0)
      return
      ;;
    1)
      if [[ "$PULL_SANDBOX" == true ]]; then
        echo "Pulling missing sandbox image $SANDBOX_IMAGE..."
        container_pull_image
      else
        die "sandbox image $SANDBOX_IMAGE is not present; you have to either --pull-sandbox or use --no-sandbox"
      fi
      ;;
    2)
      die "docker/podman not found; you have to either install one or use --no-sandbox"
      ;;
  esac
}

setup_sandbox() {
  ensure_exo_bin
  container_pull_image
}

setup_agent() {
  ensure_exo_bin
  if [[ "$USE_SANDBOX" == true ]]; then
    ensure_sandbox_image
  fi
  ensure_agent
  ensure_conversation
  ensure_self_repo_mount
  ensure_agent_cli_mount
}

agent_exists() {
  exo agent show "$AGENT" >/dev/null 2>&1
}

conversation_exists() {
  exo conversation show "$AGENT" "$CONVERSATION" >/dev/null 2>&1
}

ensure_agent() {
  if agent_exists; then
    return
  fi

  echo "Creating agent $AGENT..."
  local args=(
    --harness "$HARNESS"
    agent create "$AGENT_NAME"
    --slug "$AGENT"
    --module "$MODULE"
    --model "$MODEL"
  )
  if [[ "$USE_SANDBOX" == true ]]; then
    args+=(--sandbox-image "$SANDBOX_IMAGE" --networking "$NETWORKING")
    if [[ -n "$SANDBOX_PROVIDER" ]]; then
      args+=(--sandbox-provider "$SANDBOX_PROVIDER")
    fi
    # The exo agent shares one sandbox across all of its conversations.
    args+=(--sandbox-scope "${SANDBOX_SCOPE:-agent}")
  fi
  exo "${args[@]}"
}

ensure_conversation() {
  if conversation_exists; then
    if [[ "$USE_SANDBOX" == true && ( -n "$SANDBOX_SCOPE" || -n "$SANDBOX_PROVIDER" ) ]]; then
      local update_args=(conversation update "$AGENT" "$CONVERSATION")
      if [[ -n "$SANDBOX_SCOPE" ]]; then
        update_args+=(--sandbox-scope "$SANDBOX_SCOPE")
      fi
      if [[ -n "$SANDBOX_PROVIDER" ]]; then
        update_args+=(--sandbox-provider "$SANDBOX_PROVIDER")
      fi
      exo "${update_args[@]}" >/dev/null
    fi
    return
  fi

  echo "Creating conversation $CONVERSATION..."
  local args=(conversation create "$AGENT" "$CONVERSATION_NAME" --slug "$CONVERSATION")
  if [[ -n "$SANDBOX_SCOPE" ]]; then
    args+=(--sandbox-scope "$SANDBOX_SCOPE")
  fi
  if [[ -n "$SANDBOX_PROVIDER" ]]; then
    args+=(--sandbox-provider "$SANDBOX_PROVIDER")
  fi
  exo "${args[@]}"
  if [[ "$USE_SANDBOX" == true ]]; then
    local update_args=(conversation update "$AGENT" "$CONVERSATION" --shell-program "$SHELL_PROGRAM")
    if [[ -n "$SANDBOX_SCOPE" ]]; then
      update_args+=(--sandbox-scope "$SANDBOX_SCOPE")
    fi
    if [[ -n "$SANDBOX_PROVIDER" ]]; then
      update_args+=(--sandbox-provider "$SANDBOX_PROVIDER")
    fi
    exo "${update_args[@]}" >/dev/null
  fi
}

ensure_self_repo_mount() {
  if [[ "$USE_SANDBOX" != true ]]; then
    return
  fi
  if [[ ! "$SELF_REPO_MOUNT_PATH" = /* ]]; then
    die "self repo mount path must be absolute: $SELF_REPO_MOUNT_PATH"
  fi
  if [[ ! -f "$ROOT_DIR/examples/exo/SELF.md" ]]; then
    die "Exo self map is missing: examples/exo/SELF.md"
  fi

  # Agent-level mounts apply to the shared agent sandbox for every
  # conversation, so adapter conversations see the repo too.
  exo agent mount add "$AGENT" "$ROOT_DIR" "$SELF_REPO_MOUNT_PATH" --rw >/dev/null
}

ensure_agent_cli_mount() {
  if [[ "$USE_SANDBOX" != true || -z "$AGENT_CLI_MOUNT_ROOT" ]]; then
    return
  fi
  if [[ ! "$AGENT_CLI_MOUNT_ROOT" = /* ]]; then
    die "agent-cli mount root must be absolute: $AGENT_CLI_MOUNT_ROOT"
  fi
  if [[ ! -d "$AGENT_CLI_MOUNT_ROOT" ]]; then
    die "agent-cli mount root does not exist: $AGENT_CLI_MOUNT_ROOT"
  fi
  if [[ ! "$AGENT_CLI_MOUNT_PATH" = /* ]]; then
    die "agent-cli mount path must be absolute: $AGENT_CLI_MOUNT_PATH"
  fi

  exo agent mount add "$AGENT" "$AGENT_CLI_MOUNT_ROOT" "$AGENT_CLI_MOUNT_PATH" --rw >/dev/null
}

list_agents_and_conversations() {
  ensure_exo_bin
  echo "Agents and conversations:"
  local agents
  agents="$(exo agent list | awk 'NR > 1 { print $1 }')"
  if [[ -z "$agents" ]]; then
    echo "  none"
    return
  fi

  while IFS= read -r agent; do
    [[ -z "$agent" ]] && continue
    echo
    exo agent show "$agent" | awk '
      /^slug:/ { slug=$2 }
      /^name:/ { name=substr($0, 7) }
      END {
        if (slug != "") {
          printf "%s", slug
          if (name != "") {
            printf " - %s", name
          }
          printf "\n"
        }
      }
    '
    exo conversation list "$agent" | awk 'NR == 1 { next } { printf "  %s - %s\n", $1, $3 }'
  done <<<"$agents"
}

stop_scheduler() {
  local pid_file pid
  pid_file="$(scheduler_pid_file)"
  if [[ -f "$pid_file" ]]; then
    pid="$(<"$pid_file")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" >/dev/null 2>&1; then
      echo "Stopping Exo scheduler..."
      terminate_process_tree "$pid"
    fi
  fi
  pkill -f "exo-scheduler-runner .*run --watch" >/dev/null 2>&1 || true
  rm -f "$(scheduler_lock_file)"
  rm -f "$pid_file"
}

stop_adapters() {
  local pid_file pid
  pid_file="$(adapters_pid_file)"
  if [[ -f "$pid_file" ]]; then
    pid="$(<"$pid_file")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" >/dev/null 2>&1; then
      echo "Stopping Exo adapter runner..."
      terminate_process_tree "$pid"
    fi
  fi
  pkill -f "exo .*adapters run" >/dev/null 2>&1 || true
  pkill -f "tsx examples/exo/adapters/.*/worker.ts" >/dev/null 2>&1 || true
  rm -f "$pid_file"
}

stop_all_processes() {
  stop_scheduler
  stop_adapters
  echo "Stopped Exo scheduler and adapter runners. State in .exo was preserved."
}

delete_adapter_state() {
  stop_adapters
  rm -rf "$ROOT_DIR/.exo/adapters"
  rm -f \
    "$ROOT_DIR/.exo/exo-adapters.pid" \
    "$ROOT_DIR/.exo/exo-adapters.log" \
    "$ROOT_DIR/.exo/exo-adapters.lock"
}

delete_all_agents_and_conversations() {
  ensure_exo_bin
  stop_scheduler
  delete_adapter_state

  local agents
  agents="$(exo agent list | awk 'NR > 1 { print $1 }')"
  if [[ -z "$agents" ]]; then
    echo "No agents to delete."
    return
  fi

  while IFS= read -r agent; do
    [[ -z "$agent" ]] && continue

    local conversations
    conversations="$(exo conversation list "$agent" | awk 'NR > 1 { print $1 }')"
    while IFS= read -r conversation; do
      [[ -z "$conversation" ]] && continue
      echo "Deleting conversation $agent/$conversation..."
      exo conversation delete "$agent" "$conversation" >/dev/null
    done <<<"$conversations"

    echo "Deleting agent $agent..."
    exo agent delete "$agent" >/dev/null
  done <<<"$agents"

  echo "Deleted all agents and conversations."
}

setup_local_profile() {
  mkdir -p "$(dirname "$LOCAL_PROMPT_FILE")"

  if [[ -f "$LOCAL_PROMPT_FILE" ]]; then
    echo "Local Exo profile already exists: $LOCAL_PROMPT_FILE"
    read -r -p "Overwrite it? [y/N] " overwrite
    case "$overwrite" in
      y|Y|yes|YES) ;;
      *)
        echo "Keeping existing local profile."
        return
        ;;
    esac
  fi

  local user_name extra_instructions
  echo "Creating local Exo profile: $LOCAL_PROMPT_FILE"
  read -r -p "Your name, or blank to skip: " user_name
  read -r -p "Additional local instructions, or blank to skip: " extra_instructions

  {
    echo "# Local Exo Profile"
    echo
    echo "This file is local to this machine and should not be committed."
    if [[ -n "$user_name" ]]; then
      echo
      echo "The user's name is $user_name."
    fi
    if [[ -n "$extra_instructions" ]]; then
      echo
      echo "$extra_instructions"
    fi
  } >"$LOCAL_PROMPT_FILE"

  echo "Wrote local profile prompt. The harness will load it from EXO_LOCAL_PROMPT_FILE."
}

# During fresh, all agents and conversations from this checkout are deleted, so
# sandbox containers left behind by earlier runs of this checkout are obsolete.
# Remove exo sandbox containers whose creating process is gone, plus any exo
# sandbox container that has this repo mounted. Containers from other checkouts
# with live owners are left alone, as is anything without exo sandbox labels.
cleanup_stale_sandbox_containers() {
  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    return
  fi
  local container key owner name removed=0
  for container in $(docker ps -a --filter "name=exo-" --format '{{.ID}}' 2>/dev/null); do
    name="$(docker inspect "$container" --format '{{.Name}}' 2>/dev/null | sed 's|^/||')"
    [[ "$name" == exo-* ]] || continue
    key="$(docker inspect "$container" --format '{{index .Config.Labels "exo.sandbox.key"}}' 2>/dev/null)"
    [[ -n "$key" ]] || continue
    owner="$(docker inspect "$container" --format '{{index .Config.Labels "exo.sandbox.owner-pid"}}' 2>/dev/null || true)"
    if [[ -n "$owner" ]] && kill -0 "$owner" 2>/dev/null && ! container_mounts_current_checkout "$container"; then
      continue
    fi
    echo "Removing stale sandbox container $name..."
    docker rm -f "$container" >/dev/null 2>&1 || true
    removed=$((removed + 1))
  done
  if [[ "$removed" -gt 0 ]]; then
    echo "Removed $removed stale sandbox container(s)."
  fi
}

container_mounts_current_checkout() {
  local container="$1"
  local source
  while IFS= read -r source; do
    if [[ "$source" == "$ROOT_DIR" ]]; then
      return 0
    fi
  done < <(docker inspect "$container" --format '{{range .Mounts}}{{println .Source}}{{end}}' 2>/dev/null || true)
  return 1
}

fresh_start() {
  if [[ "$SKIP_BUILD" == true ]]; then
    ensure_exo_bin
  else
    build_exo
  fi
  delete_all_agents_and_conversations
  cleanup_stale_sandbox_containers
  if [[ "$USE_SANDBOX" == true ]]; then
    PULL_SANDBOX=true
  fi
  run_repl
}

run_repl() {
  ensure_exo_bin
  if [[ "$SETUP_PROFILE" == true ]]; then
    setup_local_profile
  fi
  ensure_signal_cli
  if [[ "$USE_SANDBOX" == true ]]; then
    ensure_sandbox_image
  fi
  ensure_agent
  ensure_conversation
  ensure_self_repo_mount
  ensure_agent_cli_mount
  configure_guardian_for_current_launch
  local scheduler_log_start_line
  scheduler_log_start_line="$(scheduler_log_line_count)"
  ensure_scheduler
  local adapter_log_start_line
  adapter_log_start_line="$(adapter_log_line_count)"
  send_startup_prompt
  ensure_adapters
  show_exochat_url_if_needed "$adapter_log_start_line"
  show_signal_qr_if_needed "$adapter_log_start_line"
  show_whatsapp_qr_if_needed "$adapter_log_start_line"
  if [[ "$CONTROL" == true ]]; then
    run_control_repl "$scheduler_log_start_line" "$adapter_log_start_line"
  else
    EXO_GLOBAL_ARGS=()
    append_exo_global_args
    exec "$EXO_BIN" "${EXO_GLOBAL_ARGS[@]}" repl \
      --agent "$AGENT" \
      --conversation "$CONVERSATION"
  fi
}

adapter_log_line_count() {
  local log_file
  log_file="$(adapters_log_file)"
  if [[ -f "$log_file" ]]; then
    wc -l <"$log_file" | tr -d '[:space:]'
  else
    echo 0
  fi
}

scheduler_log_line_count() {
  local log_file
  log_file="$(scheduler_log_file)"
  if [[ -f "$log_file" ]]; then
    wc -l <"$log_file" | tr -d '[:space:]'
  else
    echo 0
  fi
}

run_control_repl() {
  local scheduler_start_line="$1"
  local adapter_start_line="$2"
  local repl_pid=""
  local restart_watcher_pid=""

  cleanup_control_logs() {
    local pid
    for pid in "${CONTROL_PIDS[@]:-}"; do
      kill "$pid" >/dev/null 2>&1 || true
    done
    if [[ -n "${restart_watcher_pid:-}" ]]; then
      kill "$restart_watcher_pid" >/dev/null 2>&1 || true
    fi
    kill_repl_children "$$"
  }
  trap cleanup_control_logs EXIT INT TERM

  echo "Control console enabled. Streaming scheduler and adapter logs beside the REPL."
  echo "The control wrapper will restart the REPL child when $(repl_restart_file) appears."
  if [[ "$START_SCHEDULER" == true ]]; then
    start_control_log_tail "scheduler" "$(scheduler_log_file)" "$scheduler_start_line"
  fi
  if [[ "$START_ADAPTERS" == true ]]; then
    start_control_log_tail "adapters" "$(adapters_log_file)" "$adapter_start_line"
  fi

  EXO_GLOBAL_ARGS=()
  append_exo_global_args
  while true; do
    rm -f "$(repl_restart_file)"
    watch_repl_restart_request "$$" &
    restart_watcher_pid="$!"

    local repl_exit
    if "$EXO_BIN" "${EXO_GLOBAL_ARGS[@]}" repl \
      --agent "$AGENT" \
      --conversation "$CONVERSATION"; then
      repl_exit=0
    else
      repl_exit=$?
    fi

    if [[ -n "${restart_watcher_pid:-}" ]]; then
      kill "$restart_watcher_pid" >/dev/null 2>&1 || true
      wait "$restart_watcher_pid" >/dev/null 2>&1 || true
      restart_watcher_pid=""
    fi

    if [[ -f "$(repl_restart_file)" ]]; then
      rm -f "$(repl_restart_file)"
      echo "Restarting Exo REPL child after guardian rebuild request..."
      continue
    fi
    return "$repl_exit"
  done
}

kill_repl_children() {
  local control_pid="$1"
  local child_pid
  while IFS= read -r child_pid; do
    if [[ -n "$child_pid" ]]; then
      kill "$child_pid" >/dev/null 2>&1 || true
    fi
  done < <(find_repl_children "$control_pid")
}

find_repl_children() {
  local control_pid="$1"
  ps ax -o pid= -o ppid= -o command= | awk -v ppid="$control_pid" -v exo="$EXO_BIN" '
    $2 == ppid && index($0, exo) > 0 && index($0, " repl") > 0 { print $1 }
  '
}

watch_repl_restart_request() {
  local control_pid="$1"
  local marker
  marker="$(repl_restart_file)"
  while true; do
    if [[ -f "$marker" ]]; then
      echo "Guardian requested REPL child restart; stopping current child..."
      kill_repl_children "$control_pid"
      return
    fi
    sleep 2
  done
}

start_control_log_tail() {
  local label="$1"
  local log_file="$2"
  local start_line="$3"
  local tail_start=$((start_line + 1))

  mkdir -p "$(dirname "$log_file")"
  touch "$log_file"
  echo "[$label] tailing $log_file"
  tail -n +"$tail_start" -F "$log_file" 2>/dev/null \
    | awk -v label="$label" '{ print "[" label "] " $0; fflush(); }' &
  CONTROL_PIDS+=("$!")
}

startup_prompt_file() {
  local path="$1"
  if [[ ! -f "$path" ]]; then
    die "startup prompt file not found: $path"
  fi
  if [[ ! -s "$path" ]]; then
    die "startup prompt file is empty: $path"
  fi
  printf '%s\n' "$path"
}

adapter_setup_prompt_file() {
  local adapter="$1"
  if [[ ! "$adapter" =~ ^[a-zA-Z0-9_-]+$ ]]; then
    die "--setup must be an adapter name, not a path: $adapter"
  fi
  startup_prompt_file "$ROOT_DIR/examples/exo/adapters/$adapter/setup-prompt.md"
}

send_startup_prompt() {
  if [[ "${#SETUP_ADAPTERS[@]}" -eq 0 && -z "$INITIAL_PROMPT_FILE" ]]; then
    return
  fi

  local adapter
  for adapter in "${SETUP_ADAPTERS[@]}"; do
    send_adapter_setup_prompt "$adapter"
  done

  if [[ -n "$INITIAL_PROMPT_FILE" ]]; then
    send_prompt_from_files "$(startup_prompt_file "$INITIAL_PROMPT_FILE")"
  fi
}

send_adapter_setup_prompt() {
  local adapter="$1"
  local file prompt
  file="$(adapter_setup_prompt_file "$adapter")"
  prompt="$(<"$file")"

  # The agent-cli setup prompt asks for the host workspace root, which the
  # sandboxed agent cannot determine on its own. Inject it when known so the
  # non-interactive setup can complete without waiting for an answer.
  if [[ "$adapter" == "agent-cli" ]]; then
    if [[ -z "$AGENT_CLI_MOUNT_ROOT" ]]; then
      die "--setup agent-cli requires --agent-cli-mount <host-dir> so the host workspace root can be provided to the agent"
    fi
    prompt+=$'\n\n'"The host workspace root bind-mounted at $AGENT_CLI_MOUNT_PATH is: $AGENT_CLI_MOUNT_ROOT"
  fi

  if [[ -z "${prompt//[[:space:]]/}" ]]; then
    die "startup prompt is empty: $file"
  fi
  echo "Sending startup prompt from: $file"
  exo conversation send "$AGENT" "$CONVERSATION" "$prompt"
}

send_prompt_from_files() {
  local files=("$@")
  local prompt=""
  local file
  for file in "${files[@]}"; do
    prompt+=$'\n\n'
    prompt+="$(<"$file")"
  done
  prompt="${prompt#$'\n\n'}"
  if [[ -z "${prompt//[[:space:]]/}" ]]; then
    die "startup prompt is empty"
  fi

  echo "Sending startup prompt from: ${files[*]}"
  exo conversation send "$AGENT" "$CONVERSATION" "$prompt"
}

show_whatsapp_qr_if_needed() {
  if ! setup_requested "whatsapp"; then
    return
  fi

  local start_line="${1:-0}"
  local log_file
  log_file="$(adapters_log_file)"
  echo "Waiting for WhatsApp QR code in $log_file..."

  local attempt qr
  for attempt in {1..30}; do
    if [[ -f "$log_file" ]]; then
      qr="$(awk -v start="$start_line" '
        NR <= start { next }
        /\[whatsapp-adapter\] Scan this QR with WhatsApp:/ {
          capture = 1
          block = $0 "\n"
          next
        }
        capture {
          if ($0 ~ /^\[[^]]+-adapter\]/) {
            capture = 0
            next
          }
          block = block $0 "\n"
        }
        END {
          if (block != "") {
            printf "%s", block
          }
        }
      ' "$log_file")"
      if [[ -n "$qr" ]]; then
        echo
        printf '%s\n' "$qr"
        echo "Scan this QR from WhatsApp: Settings > Linked devices > Link a device."
        echo
        read -r -p "Press Enter after WhatsApp finishes linking the device..."
        return
      fi
    fi
    sleep 1
  done

  echo "No WhatsApp QR code found yet. The adapter may already be paired, or it may still be starting."
  echo "Watch the adapter log with: tail -f $log_file"
  read -r -p "Press Enter to continue to the REPL..."
}

show_exochat_url_if_needed() {
  if ! setup_requested "exochat"; then
    return
  fi

  local start_line="${1:-0}"
  local log_file
  log_file="$(adapters_log_file)"
  echo "Waiting for ExoChat URL in $log_file..."

  local attempt url
  for attempt in {1..30}; do
    if [[ -f "$log_file" ]]; then
      url="$(awk -v start="$start_line" '
        NR <= start { next }
        /\[exochat-adapter\] Open this ExoChat URL/ {
          capture = 1
          next
        }
        capture && /^https?:\/\// {
          print
          exit
        }
      ' "$log_file")"
      if [[ -n "$url" ]]; then
        echo
        echo "Open this ExoChat URL in a browser or on your phone:"
        printf '%s\n' "$url"
        echo
        read -r -p "Press Enter after opening ExoChat..."
        return
      fi
    fi
    sleep 1
  done

  echo "No ExoChat URL found yet. Watch the adapter log with: tail -f $log_file"
  read -r -p "Press Enter to continue to the REPL..."
}

show_signal_qr_if_needed() {
  if ! setup_requested "signal"; then
    return
  fi

  local start_line="${1:-0}"
  local log_file
  log_file="$(adapters_log_file)"
  echo "Waiting for Signal linked-device QR code in $log_file..."

  local attempt qr
  for attempt in {1..30}; do
    if [[ -f "$log_file" ]]; then
      qr="$(awk -v start="$start_line" '
        NR <= start { next }
        /\[signal-adapter\] Scan this QR with Signal:/ {
          capture = 1
          block = $0 "\n"
          next
        }
        capture {
          if ($0 ~ /^\[[^]]+-adapter\]/) {
            capture = 0
            next
          }
          block = block $0 "\n"
        }
        END {
          if (block != "") {
            printf "%s", block
          }
        }
      ' "$log_file")"
      if [[ -n "$qr" ]]; then
        echo
        printf '%s\n' "$qr"
        echo "Scan this QR from Signal: Settings > Linked devices > Link new device."
        echo
        read -r -p "Press Enter after Signal finishes linking the device..."
        return
      fi
    fi
    sleep 1
  done

  echo "No Signal QR code found yet. The adapter may already be linked, signal-cli may be missing, or it may still be starting."
  echo "Watch the adapter log with: tail -f $log_file"
  read -r -p "Press Enter to continue to the REPL..."
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    list)
      shift
      [[ $# -eq 0 ]] || die "list does not accept additional arguments"
      COMMAND="list"
      ;;
    delall|delete-all)
      shift
      [[ "${1:-}" == "all" ]] || die "delall requires literal argument: all"
      shift
      [[ $# -eq 0 ]] || die "delall all does not accept additional arguments"
      COMMAND="delall"
      ;;
    fresh|reset)
      shift
      COMMAND="fresh"
      ;;
    stop-all|stop)
      shift
      [[ $# -eq 0 ]] || die "stop-all does not accept additional arguments"
      COMMAND="stop-all"
      ;;
    setup-profile)
      shift
      COMMAND="setup-profile"
      ;;
    setup-sandbox)
      shift
      COMMAND="setup-sandbox"
      ;;
    setup-agent)
      shift
      COMMAND="setup-agent"
      ;;
    build)
      shift
      [[ $# -eq 0 ]] || die "build does not accept additional arguments"
      COMMAND="build"
      ;;
    register-model)
      shift
      COMMAND="register-model"
      ;;
    write-profile)
      shift
      COMMAND="write-profile"
      ;;
    --model)
      MODEL="${2:-}"
      [[ -n "$MODEL" ]] || die "--model requires a value"
      shift 2
      ;;
    --upstream-model)
      UPSTREAM_MODEL="${2:-}"
      [[ -n "$UPSTREAM_MODEL" ]] || die "--upstream-model requires a value"
      shift 2
      ;;
    --secret-name)
      SECRET_NAME="${2:-}"
      [[ -n "$SECRET_NAME" ]] || die "--secret-name requires a value"
      shift 2
      ;;
    --secret-env)
      SECRET_ENV="${2:-}"
      [[ -n "$SECRET_ENV" ]] || die "--secret-env requires a value"
      shift 2
      ;;
    --base-url)
      MODEL_BASE_URL="${2:-}"
      [[ -n "$MODEL_BASE_URL" ]] || die "--base-url requires a value"
      shift 2
      ;;
    --user-name)
      USER_NAME="${2:-}"
      shift 2
      ;;
    --agent)
      AGENT="${2:-}"
      [[ -n "$AGENT" ]] || die "--agent requires a value"
      shift 2
      ;;
    --conversation|--convo)
      CONVERSATION="${2:-}"
      [[ -n "$CONVERSATION" ]] || die "$1 requires a value"
      shift 2
      ;;
    --agent-name)
      AGENT_NAME="${2:-}"
      [[ -n "$AGENT_NAME" ]] || die "--agent-name requires a value"
      shift 2
      ;;
    --conversation-name)
      CONVERSATION_NAME="${2:-}"
      [[ -n "$CONVERSATION_NAME" ]] || die "--conversation-name requires a value"
      shift 2
      ;;
    --module)
      MODULE="${2:-}"
      [[ -n "$MODULE" ]] || die "--module requires a value"
      shift 2
      ;;
    --sandbox-image)
      SANDBOX_IMAGE="${2:-}"
      [[ -n "$SANDBOX_IMAGE" ]] || die "--sandbox-image requires a value"
      shift 2
      ;;
    --sandbox-provider)
      SANDBOX_PROVIDER="${2:-}"
      case "$SANDBOX_PROVIDER" in
        daytona|apple-container|docker|local-process) ;;
        *) die "--sandbox-provider must be daytona, apple-container, docker, or local-process" ;;
      esac
      SANDBOX_PROVIDER_EXPLICIT=true
      shift 2
      ;;
    --sandbox-backend)
      SANDBOX_BACKEND="${2:-}"
      case "$SANDBOX_BACKEND" in
        apple-container|docker|local-process) ;;
        *) die "--sandbox-backend must be apple-container, docker, or local-process" ;;
      esac
      SANDBOX_BACKEND_EXPLICIT=true
      shift 2
      ;;
    --template)
      TEMPLATE="${2:-}"
      case "$TEMPLATE" in
        canonical|dev|minimal) ;;
        *) die "--template must be canonical, dev, or minimal" ;;
      esac
      shift 2
      ;;
    --template=*)
      TEMPLATE="${1#--template=}"
      case "$TEMPLATE" in
        canonical|dev|minimal) ;;
        *) die "--template must be canonical, dev, or minimal" ;;
      esac
      shift
      ;;
    --self-repo-mount)
      SELF_REPO_MOUNT_PATH="${2:-}"
      [[ -n "$SELF_REPO_MOUNT_PATH" ]] || die "--self-repo-mount requires a value"
      SELF_MAP_PATH="$SELF_REPO_MOUNT_PATH/examples/exo/SELF.md"
      shift 2
      ;;
    --agent-cli-mount)
      AGENT_CLI_MOUNT_ROOT="${2:-}"
      [[ -n "$AGENT_CLI_MOUNT_ROOT" ]] || die "--agent-cli-mount requires a value"
      shift 2
      ;;
    --agent-cli-mount-path)
      AGENT_CLI_MOUNT_PATH="${2:-}"
      [[ -n "$AGENT_CLI_MOUNT_PATH" ]] || die "--agent-cli-mount-path requires a value"
      shift 2
      ;;
    --networking)
      NETWORKING="${2:-}"
      [[ "$NETWORKING" == "enabled" || "$NETWORKING" == "disabled" ]] || die "--networking must be enabled or disabled"
      shift 2
      ;;
    --shell-program)
      SHELL_PROGRAM="${2:-}"
      [[ -n "$SHELL_PROGRAM" ]] || die "--shell-program requires a value"
      shift 2
      ;;
    --sandbox-scope)
      SANDBOX_SCOPE="${2:-}"
      [[ "$SANDBOX_SCOPE" == "agent" || "$SANDBOX_SCOPE" == "conversation" ]] || die "--sandbox-scope must be agent or conversation"
      shift 2
      ;;
    --scheduler-interval)
      SCHEDULER_INTERVAL_SECONDS="${2:-}"
      [[ "$SCHEDULER_INTERVAL_SECONDS" =~ ^[0-9]+$ && "$SCHEDULER_INTERVAL_SECONDS" -gt 0 ]] || die "--scheduler-interval requires a positive integer"
      shift 2
      ;;
    --no-scheduler)
      START_SCHEDULER=false
      shift
      ;;
    --scheduler)
      START_SCHEDULER=true
      shift
      ;;
    --no-adapters)
      START_ADAPTERS=false
      shift
      ;;
    --adapters)
      START_ADAPTERS=true
      shift
      ;;
    --adapter-limit)
      ADAPTER_LIMIT="${2:-}"
      [[ "$ADAPTER_LIMIT" =~ ^[0-9]+$ && "$ADAPTER_LIMIT" -gt 0 ]] || die "--adapter-limit requires a positive integer"
      shift 2
      ;;
    --control)
      CONTROL=true
      shift
      ;;
    --setup-profile)
      SETUP_PROFILE=true
      shift
      ;;
    --local-prompt-file)
      LOCAL_PROMPT_FILE="${2:-}"
      [[ -n "$LOCAL_PROMPT_FILE" ]] || die "--local-prompt-file requires a value"
      shift 2
      ;;
    --setup)
      add_setup_adapter "${2:-}"
      shift 2
      ;;
    --setup-all)
      add_setup_adapter "exochat"
      add_setup_adapter "signal"
      add_setup_adapter "whatsapp"
      add_setup_adapter "irc"
      add_setup_adapter "discord"
      shift
      ;;
    --initial-prompt-file)
      INITIAL_PROMPT_FILE="${2:-}"
      [[ -n "$INITIAL_PROMPT_FILE" ]] || die "--initial-prompt-file requires a value"
      shift 2
      ;;
    --pull-sandbox)
      PULL_SANDBOX=true
      shift
      ;;
    --skip-build)
      SKIP_BUILD=true
      shift
      ;;
    --no-sandbox)
      USE_SANDBOX=false
      shift
      ;;
    --env-file)
      ENV_FILE="${2:-}"
      [[ -n "$ENV_FILE" ]] || die "--env-file requires a value"
      shift 2
      ;;
    --exo-bin)
      EXO_BIN="${2:-}"
      [[ -n "$EXO_BIN" ]] || die "--exo-bin requires a value"
      shift 2
      ;;
    --scheduler-bin)
      SCHEDULER_BIN="${2:-}"
      [[ -n "$SCHEDULER_BIN" ]] || die "--scheduler-bin requires a value"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

apply_template_defaults

export EXO_LOCAL_PROMPT_FILE="$LOCAL_PROMPT_FILE"
export EXO_REPO="$SELF_REPO_MOUNT_PATH"
export EXO_SELF_MAP="$SELF_MAP_PATH"

case "$COMMAND" in
  repl)
    run_repl
    ;;
  list)
    list_agents_and_conversations
    ;;
  delall)
    delete_all_agents_and_conversations
    ;;
  fresh)
    fresh_start
    ;;
  stop-all)
    stop_all_processes
    ;;
  setup-profile)
    setup_local_profile
    ;;
  setup-sandbox)
    setup_sandbox
    ;;
  setup-agent)
    setup_agent
    ;;
  build)
    build_all
    ;;
  register-model)
    register_model
    ;;
  write-profile)
    write_local_profile
    ;;
esac
