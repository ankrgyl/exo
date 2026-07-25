#!/usr/bin/env bash
set -euo pipefail

REPO_URL="${EXO_REPO_URL:-https://github.com/exoharness/exo.git}"
REPO_REF="${EXO_REPO_REF:-main}"
INSTALL_DIR="${EXO_INSTALL_DIR:-$PWD}"
MODEL_NAME="${EXO_MODEL:-gpt-5.6-terra}"
UPSTREAM_MODEL="${EXO_UPSTREAM_MODEL:-}"
MODEL_PROVIDER="${EXO_MODEL_PROVIDER:-}"
AGENT_NAME="${EXO_AGENT_NAME:-Exo}"
USER_NAME="${EXO_USER_NAME:-}"
FORCE_INSTALL="${EXO_SETUP_FORCE:-false}"
INSTALL_DEPS="${EXO_SETUP_INSTALL_DEPS:-false}"
DOCKER_GROUP_ADDED=false
SETUP_ARGS=()
DEFAULT_EXO_CHAT_BASE_URL="https://exoharness.ai"
DEFAULT_OPENROUTER_BASE_URL="https://openrouter.ai/api/v1"
DEFAULT_OPENROUTER_MODEL="z-ai/glm-5.2"

die() {
  echo "error: $*" >&2
  exit 1
}

info() {
  echo
  echo "==> $*"
}

usage() {
  cat <<'EOF'
Usage:
  bash setup.sh [options]

Options:
  --branch <branch>     Git branch to clone for testing (default: main)
  --ref <ref>           Git ref to clone; alias for --branch
  --repo-ref <ref>      Git ref to clone; alias for --branch
  --force               Back up existing files and install into a non-empty
                        directory; refuses to run directly in $HOME or /
  --install-deps        Install missing dependencies without prompting
  --help                Show this help

Environment overrides:
  EXO_REPO_URL, EXO_REPO_REF, EXO_INSTALL_DIR, EXO_MODEL_PROVIDER, EXO_MODEL,
  EXO_UPSTREAM_MODEL, EXO_AGENT_NAME, EXO_USER_NAME, EXO_CHAT_BASE_URL,
  EXO_LOCAL_PROMPT_FILE, EXO_SETUP_FORCE, EXO_SETUP_INSTALL_DEPS,
  OPENAI_API_KEY, OPENROUTER_API_KEY
EOF
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --branch|--ref|--repo-ref)
        REPO_REF="${2:-}"
        [[ -n "$REPO_REF" ]] || die "$1 requires a value"
        shift 2
        ;;
      --force)
        FORCE_INSTALL=true
        shift
        ;;
      --install-deps)
        INSTALL_DEPS=true
        shift
        ;;
      --help|-h)
        usage
        exit 0
        ;;
      *)
        die "unknown option: $1"
        ;;
    esac
  done
}

require_command() {
  local command="$1"
  local install_hint="$2"
  if ! command -v "$command" >/dev/null 2>&1; then
    die "$command is required. $install_hint"
  fi
}

source_cargo_env() {
  if [[ -f "$HOME/.cargo/env" ]]; then
    # Pick up a rustup-managed toolchain that is not on PATH yet.
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
}

# git, docker, and (on Linux) a C toolchain must be installed by the system
# package manager. node, pnpm, and rust are installed by mise after clone,
# pinned to the versions in mise.toml, when not already present.
collect_missing_dependencies() {
  MISSING_DEPS=()
  SYSTEM_MISSING=()

  if ! command -v git >/dev/null 2>&1; then
    MISSING_DEPS+=("git")
    SYSTEM_MISSING+=("git")
  fi
  if ! command -v docker >/dev/null 2>&1; then
    MISSING_DEPS+=("docker")
    SYSTEM_MISSING+=("docker")
  fi
  if [[ "$(uname -s)" == "Linux" ]] && ! command -v cc >/dev/null 2>&1; then
    MISSING_DEPS+=("build-tools")
    SYSTEM_MISSING+=("build-tools")
  fi
  command -v node >/dev/null 2>&1 ||
    MISSING_DEPS+=("node")
  command -v pnpm >/dev/null 2>&1 ||
    MISSING_DEPS+=("pnpm")
  if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
    MISSING_DEPS+=("rust")
  fi
}

print_dependency_status() {
  local dep
  echo "Exo uses these dependencies:"
  for dep in git node pnpm rust docker; do
    if missing_has "$dep" "${MISSING_DEPS[@]}"; then
      echo "  - $dep (missing)"
    else
      echo "  - $dep (installed)"
    fi
  done
  if missing_has build-tools "${MISSING_DEPS[@]}"; then
    echo "  - C build tools (missing)"
  fi
}

mise_available() {
  command -v mise >/dev/null 2>&1 || [[ -x "$HOME/.local/bin/mise" ]]
}

check_dependencies() {
  source_cargo_env
  collect_missing_dependencies
  if ((${#MISSING_DEPS[@]} == 0)); then
    return
  fi
  if ((${#SYSTEM_MISSING[@]} == 0)) && mise_available; then
    # Only the mise-managed toolchains are missing; no prompt needed.
    echo "node, pnpm, and rust will be installed with mise once the repository is in place."
    return
  fi

  echo
  print_dependency_status

  local mode
  mode="$(choose_dependency_install_mode)"
  if [[ "$mode" == "manual" ]]; then
    print_dependency_install_help "${MISSING_DEPS[@]}"
    exit 1
  fi

  if ((${#SYSTEM_MISSING[@]} > 0)); then
    install_missing_dependencies "${SYSTEM_MISSING[@]}"
    hash -r
  fi
  collect_missing_dependencies
  if ((${#SYSTEM_MISSING[@]} > 0)); then
    echo "Still missing after the install flow: ${SYSTEM_MISSING[*]}" >&2
    print_dependency_install_help "${MISSING_DEPS[@]}"
    exit 1
  fi
  if missing_has node "${MISSING_DEPS[@]}" || missing_has pnpm "${MISSING_DEPS[@]}" ||
    missing_has rust "${MISSING_DEPS[@]}"; then
    echo "node, pnpm, and rust will be installed with mise once the repository is in place."
  fi
  maybe_reexec_for_docker_group
}

can_auto_install_dependencies() {
  case "$(uname -s)" in
    Darwin) return 0 ;;
    Linux) command -v apt-get >/dev/null 2>&1 ;;
    *) return 1 ;;
  esac
}

choose_dependency_install_mode() {
  if [[ "$INSTALL_DEPS" == true ]] && can_auto_install_dependencies; then
    printf '%s' auto
    return
  fi
  if [[ ! -t 0 ]] || ! can_auto_install_dependencies; then
    printf '%s' manual
    return
  fi
  echo "How should the missing dependencies be installed?" >&2
  echo "1) Automatically (recommended): the system package manager installs git and Docker; mise (https://mise.jdx.dev) installs pinned node, pnpm, and rust" >&2
  echo "2) Manually: print the install commands for each and exit" >&2
  local choice
  while true; do
    read -r -p "Install mode [1-2, default 1]: " choice
    case "${choice:-1}" in
      1) printf '%s' auto; return ;;
      2) printf '%s' manual; return ;;
      *) echo "Please choose 1 or 2." >&2 ;;
    esac
  done
}

sudo_run() {
  if [[ "$(id -u)" == "0" ]]; then
    "$@"
  else
    command -v sudo >/dev/null 2>&1 ||
      die "sudo is required to install dependencies automatically. Install them manually and rerun."
    sudo "$@"
  fi
}

install_missing_dependencies() {
  case "$(uname -s)" in
    Darwin) install_missing_dependencies_macos "$@" ;;
    Linux) install_missing_dependencies_linux "$@" ;;
    *) die "automatic dependency install is not supported on this platform" ;;
  esac
}

install_missing_dependencies_linux() {
  info "Installing dependencies with apt-get"
  sudo_run apt-get update
  local apt_packages=("curl" "ca-certificates")
  if missing_has git "$@"; then
    apt_packages+=("git")
  fi
  if missing_has build-tools "$@"; then
    apt_packages+=("build-essential" "pkg-config" "libssl-dev")
  fi
  sudo_run apt-get install -y "${apt_packages[@]}"
  if missing_has docker "$@"; then
    info "Installing Docker Engine (get.docker.com)"
    curl -fsSL https://get.docker.com | sudo_run sh
    sudo_run systemctl enable --now docker 2>/dev/null || true
    if [[ "$(id -u)" != "0" ]]; then
      sudo_run usermod -aG docker "$(id -un)"
      DOCKER_GROUP_ADDED=true
    fi
  fi
}

install_missing_dependencies_macos() {
  if ! command -v brew >/dev/null 2>&1; then
    info "Installing Homebrew"
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    if [[ -x /opt/homebrew/bin/brew ]]; then
      eval "$(/opt/homebrew/bin/brew shellenv)"
    elif [[ -x /usr/local/bin/brew ]]; then
      eval "$(/usr/local/bin/brew shellenv)"
    fi
    command -v brew >/dev/null 2>&1 ||
      die "Homebrew install did not complete; install it manually and rerun."
  fi
  if missing_has git "$@"; then
    info "Installing git with Homebrew"
    brew install git
  fi
  if missing_has docker "$@"; then
    info "Installing Docker Desktop"
    brew install --cask docker
    open -a Docker || true
  fi
}

ensure_toolchains() {
  source_cargo_env
  if ! command -v mise >/dev/null 2>&1 && [[ -x "$HOME/.local/bin/mise" ]]; then
    # mise was installed manually but is not on PATH yet.
    export PATH="$HOME/.local/bin:$PATH"
  fi
  if command -v node >/dev/null 2>&1 && command -v pnpm >/dev/null 2>&1 &&
    command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
    return
  fi
  info "Installing node, pnpm, and rust with mise (versions pinned in mise.toml)"
  if ! command -v mise >/dev/null 2>&1; then
    curl -fsSL https://mise.run | sh
    export PATH="$HOME/.local/bin:$PATH"
    command -v mise >/dev/null 2>&1 ||
      die "mise install failed; see https://mise.jdx.dev/getting-started.html"
  fi
  mise trust mise.toml
  mise install
  eval "$(mise activate bash --shims)"
  local tool
  for tool in node pnpm cargo rustc; do
    command -v "$tool" >/dev/null 2>&1 ||
      die "$tool is still unavailable after mise install"
  done
}

maybe_reexec_for_docker_group() {
  if [[ "$DOCKER_GROUP_ADDED" != true ]]; then
    return
  fi
  if docker info >/dev/null 2>&1; then
    return
  fi
  # Group membership from usermod does not apply to the current shell.
  if command -v sg >/dev/null 2>&1; then
    echo "Re-running setup with the docker group applied..."
    exec sg docker -c "EXO_SETUP_INSTALL_DEPS=true bash '$0' ${SETUP_ARGS[*]:-}"
  fi
  echo "You were added to the docker group, but it requires a new login session." >&2
  echo "Log out and back in (or run: newgrp docker), then rerun this script." >&2
  exit 1
}

print_dependency_install_help() {
  echo >&2
  echo "Manual install steps for the missing dependencies:" >&2
  case "$(uname -s)" in
    Darwin)
      print_macos_dependency_install_help "$@"
      ;;
    Linux)
      print_linux_dependency_install_help "$@"
      ;;
    *)
      print_generic_dependency_install_help "$@"
      ;;
  esac
  print_toolchain_install_help "$@"
  echo >&2
  echo "Final step: rerun this script (bash setup.sh) and it will pick up from here." >&2
}

print_toolchain_install_help() {
  if ! missing_has node "$@" && ! missing_has pnpm "$@" && ! missing_has rust "$@"; then
    return
  fi
  echo >&2
  echo "node, pnpm, rust — install mise, and setup will install the pinned versions on rerun:" >&2
  echo "  curl -fsSL https://mise.run | sh" >&2
  echo '  export PATH="$HOME/.local/bin:$PATH"' >&2
  echo "  # or install node 22+, pnpm, and rustup yourself if you prefer" >&2
}

print_macos_dependency_install_help() {
  echo >&2
  echo "macOS install commands:" >&2
  if ! command -v brew >/dev/null 2>&1; then
    echo "  # Install Homebrew first if you do not have it:" >&2
    echo '  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"' >&2
  fi
  if missing_has git "$@"; then
    echo "  xcode-select --install  # includes Git, if Apple developer tools are missing" >&2
  fi
  if missing_has docker "$@"; then
    echo "  brew install --cask docker" >&2
    echo "  open -a Docker" >&2
  fi
}

print_linux_dependency_install_help() {
  echo >&2
  echo "Linux install commands:" >&2
  echo "  # Ubuntu/Debian example:" >&2
  echo "  sudo apt-get update" >&2
  local apt_packages=()
  if missing_has git "$@"; then
    apt_packages+=("git")
  fi
  if missing_has build-tools "$@"; then
    apt_packages+=("build-essential" "pkg-config" "libssl-dev")
  fi
  if ((${#apt_packages[@]} > 0)); then
    echo "  sudo apt-get install -y ${apt_packages[*]}" >&2
  fi
  if missing_has docker "$@"; then
    echo "  # Install Docker Engine for your distro, then start it:" >&2
    echo "  # https://docs.docker.com/engine/install/" >&2
  fi
}

print_generic_dependency_install_help() {
  echo >&2
  echo "Install Git and Docker Desktop/Engine (node, pnpm, and rust are handled by mise during setup)." >&2
  echo "  Git: https://git-scm.com/downloads" >&2
  echo "  Docker: https://www.docker.com/products/docker-desktop/" >&2
}

missing_has() {
  local needle="$1"
  shift
  local item
  for item in "$@"; do
    if [[ "$item" == "$needle" ]]; then
      return 0
    fi
  done
  return 1
}

is_exo_checkout() {
  local dir="$1"
  # In linked Git worktrees, .git is a file rather than a directory.
  [[ -e "$dir/.git" && -f "$dir/exo.sh" ]]
}

prompt_yes_no() {
  local prompt="$1"
  local default="$2"
  local suffix
  local answer
  case "$default" in
    y|Y) suffix="Y/n" ;;
    n|N) suffix="y/N" ;;
    *) die "invalid yes/no default: $default" ;;
  esac
  while true; do
    read -r -p "$prompt [$suffix]: " answer
    answer="${answer:-$default}"
    case "$answer" in
      y|Y|yes|YES) return 0 ;;
      n|N|no|NO) return 1 ;;
      *) echo "Please answer yes or no." ;;
    esac
  done
}

read_secret() {
  local prompt="$1"
  local value=""
  local char
  printf '%s: ' "$prompt" >&2
  while IFS= read -r -s -n1 char; do
    if [[ -z "$char" ]]; then # Enter
      break
    fi
    if [[ "$char" == $'\x7f' || "$char" == $'\x08' ]]; then # Backspace
      if [[ -n "$value" ]]; then
        value="${value%?}"
        printf '\b \b' >&2
      fi
      continue
    fi
    value+="$char"
    printf '*' >&2
  done
  echo >&2
  printf '%s' "$value"
}

prompt_text() {
  local prompt="$1"
  local default="$2"
  local value
  read -r -p "$prompt [$default]: " value
  printf '%s' "${value:-$default}"
}

choose_model_provider() {
  echo "Choose how Exo should access a model:" >&2
  echo "1) ChatGPT subscription" >&2
  echo "2) OpenAI API key" >&2
  echo "3) OpenRouter API key" >&2
  local choice
  while true; do
    read -r -p "Provider [1-3, default 1]: " choice
    case "${choice:-1}" in
      1) printf '%s' openai-chatgpt; return ;;
      2) printf '%s' openai; return ;;
      3) printf '%s' openrouter; return ;;
      *) echo "Please choose 1, 2, or 3." >&2 ;;
    esac
  done
}

configure_model_provider() {
  local provider="$1"
  case "$provider" in
    openai-chatgpt)
      MODEL_PROVIDER_LABEL="ChatGPT subscription"
      MODEL_API_KEY_ENV=""
      MODEL_BASE_URL=""
      DEFAULT_UPSTREAM_MODEL="$MODEL_NAME"
      ;;
    openai)
      MODEL_PROVIDER_LABEL="OpenAI"
      MODEL_API_KEY_ENV="OPENAI_API_KEY"
      MODEL_BASE_URL=""
      DEFAULT_UPSTREAM_MODEL="$MODEL_NAME"
      ;;
    openrouter)
      MODEL_PROVIDER_LABEL="OpenRouter"
      MODEL_API_KEY_ENV="OPENROUTER_API_KEY"
      MODEL_BASE_URL="$DEFAULT_OPENROUTER_BASE_URL"
      DEFAULT_UPSTREAM_MODEL="$DEFAULT_OPENROUTER_MODEL"
      ;;
    *)
      die "unsupported model provider: $provider" \
        "(expected openai-chatgpt, openai, or openrouter)"
      ;;
  esac
}

env_value() {
  local key="$1"
  local file="$2"
  if [[ ! -f "$file" ]]; then
    return
  fi
  awk -F= -v key="$key" '$1 == key { print substr($0, length(key) + 2); found = 1; exit } END { if (!found) exit 1 }' "$file" 2>/dev/null || true
}

set_env_value() {
  local key="$1"
  local value="$2"
  local file="$3"
  local tmp
  [[ "$value" != *$'\n'* ]] || die "$key cannot contain newlines"
  tmp="$(mktemp)"
  if [[ -f "$file" ]] && grep -qE "^${key}=" "$file"; then
    awk -v key="$key" -v value="$value" '
      BEGIN { updated = 0 }
      $0 ~ "^" key "=" {
        print key "=" value
        updated = 1
        next
      }
      { print }
      END {
        if (!updated) {
          print key "=" value
        }
      }
    ' "$file" >"$tmp"
  else
    [[ -f "$file" ]] && cp "$file" "$tmp"
    if [[ -s "$tmp" ]]; then
      printf '\n' >>"$tmp"
    fi
    printf '%s=%s\n' "$key" "$value" >>"$tmp"
  fi
  mv "$tmp" "$file"
  chmod 600 "$file"
}

set_env_default() {
  local key="$1"
  local value="$2"
  local file="$3"
  local existing
  existing="$(env_value "$key" "$file")"
  if [[ -n "$existing" ]]; then
    echo "$key is already set in .env; using it."
    return
  fi
  set_env_value "$key" "$value" "$file"
}

prompt_env_secret() {
  local key="$1"
  local file="$2"
  local description="$3"
  local required="$4"
  local existing
  local env_value
  local value
  existing="$(env_value "$key" "$file")"
  if [[ -n "$existing" ]]; then
    echo "$key is already set in .env; using it."
    return
  fi
  env_value="${!key:-}"
  if [[ -n "$env_value" ]]; then
    if prompt_yes_no "$key is set in your shell environment. Use it for .env?" y; then
      set_env_value "$key" "$env_value" "$file"
      return
    fi
  fi
  while true; do
    value="$(read_secret "$description")"
    if [[ -n "$value" || "$required" != true ]]; then
      break
    fi
    echo "$key is required for the default Exo model setup."
  done
  if [[ -n "$value" ]]; then
    set_env_value "$key" "$value" "$file"
  fi
}

ensure_docker_running() {
  require_command docker "Install Docker Desktop: https://www.docker.com/products/docker-desktop/"
  if docker info >/dev/null 2>&1; then
    return
  fi
  if [[ "$(uname -s)" == "Darwin" ]] && command -v open >/dev/null 2>&1; then
    echo "Docker does not appear to be running. Opening Docker Desktop..."
    open -a Docker || true
    for _ in $(seq 1 60); do
      if docker info >/dev/null 2>&1; then
        return
      fi
      sleep 2
    done
    die "Docker is not running. Start Docker Desktop, then rerun this script."
  fi
  # Linux: docker is installed but not reachable. If the daemon runs and the
  # user is just missing docker group membership, offer to fix that.
  if [[ "$(id -u)" != "0" ]] && getent group docker >/dev/null 2>&1 &&
    ! id -nG | grep -qw docker; then
    echo "Docker is installed, but your user cannot reach the Docker daemon (not in the docker group)."
    if [[ "$INSTALL_DEPS" == true ]] || { [[ -t 0 ]] && prompt_yes_no "Add $(id -un) to the docker group now?" y; }; then
      sudo_run usermod -aG docker "$(id -un)"
      DOCKER_GROUP_ADDED=true
      maybe_reexec_for_docker_group
      docker info >/dev/null 2>&1 && return
    fi
  fi
  sudo_run systemctl start docker 2>/dev/null || true
  if docker info >/dev/null 2>&1; then
    return
  fi
  die "Docker is installed but the daemon is not reachable. Start it (e.g. sudo systemctl start docker), then rerun this script."
}

trust_mise_config() {
  local config="$1"
  if [[ ! -f "$config" ]] || ! command -v mise >/dev/null 2>&1; then
    return
  fi
  echo "Trusting local mise config: $config"
  mise trust "$config"
}

choose_install_dir() {
  if is_exo_checkout "$PWD"; then
    echo "Using current Exo checkout: $PWD" >&2
    printf '%s' "$PWD"
    return
  fi
  if [[ -n "${EXO_INSTALL_DIR:-}" ]]; then
    printf '%s' "$INSTALL_DIR"
    return
  fi
  if [[ "$FORCE_INSTALL" == true ]] || directory_can_receive_checkout "$PWD"; then
    printf '%s' "$PWD"
    return
  fi
  if [[ ! -t 0 ]]; then
    printf '%s' "$PWD"
    return
  fi
  echo "The current directory ($PWD) is not empty, so Exo needs its own directory." >&2
  local dir
  while true; do
    dir="$(prompt_text "Where should Exo be installed?" "$PWD/exo")"
    dir="${dir/#\~\//$HOME/}"
    if is_exo_checkout "$dir"; then
      echo "Using existing Exo checkout: $dir" >&2
      printf '%s' "$dir"
      return
    fi
    if [[ ! -e "$dir" ]] || directory_can_receive_checkout "$dir"; then
      printf '%s' "$dir"
      return
    fi
    echo "$dir already exists and is not empty; choose another directory." >&2
  done
}

directory_can_receive_checkout() {
  local dir="$1"
  local entry
  shopt -s nullglob dotglob
  for entry in "$dir"/*; do
    case "$(basename "$entry")" in
      .|..|setup.sh|.DS_Store) ;;
      *) return 1 ;;
    esac
  done
  return 0
}

prepare_non_empty_install_dir() {
  local dir="$1"
  if directory_can_receive_checkout "$dir"; then
    return
  fi
  if [[ "$FORCE_INSTALL" != true ]]; then
    echo "error: $dir is not empty, and setup will not overwrite existing files." >&2
    echo >&2
    echo "To continue, choose one of:" >&2
    echo "  - Run setup from a new empty directory:" >&2
    echo "      mkdir -p ~/exo && cd ~/exo && bash /path/to/setup.sh" >&2
    echo "  - Or set an explicit install directory:" >&2
    echo "      EXO_INSTALL_DIR=~/exo bash setup.sh" >&2
    echo "  - Or, if this is a failed/throwaway setup directory, rerun with:" >&2
    echo "      bash setup.sh --force" >&2
    exit 1
  fi
  backup_existing_install_dir "$dir"
}

backup_existing_install_dir() {
  local dir="$1"
  local resolved_dir
  resolved_dir="$(cd "$dir" && pwd -P)"
  if [[ "$resolved_dir" == "/" || "$resolved_dir" == "$HOME" ]]; then
    die "refusing --force in $resolved_dir. Create an empty directory or set EXO_INSTALL_DIR instead."
  fi

  local backup_dir entry name parent_dir base_name
  parent_dir="$(dirname "$resolved_dir")"
  base_name="$(basename "$resolved_dir")"
  backup_dir="$parent_dir/$base_name.exo-setup-backup-$(date +%Y%m%d%H%M%S)"
  mkdir -p "$backup_dir"
  shopt -s nullglob dotglob
  for entry in "$dir"/*; do
    name="$(basename "$entry")"
    case "$name" in
      .|..|setup.sh|.DS_Store) ;;
      *) mv "$entry" "$backup_dir/" ;;
    esac
  done

  if ! directory_can_receive_checkout "$dir"; then
    die "could not prepare $dir for checkout after backing up existing files"
  fi
  echo "Backed up existing files to $backup_dir"
}

clone_or_reuse_repo() {
  local install_dir="$1"
  local tmp_parent
  local tmp_checkout
  if is_exo_checkout "$install_dir"; then
    echo "Using existing Exo checkout at $install_dir"
    return
  fi
  mkdir -p "$install_dir"
  prepare_non_empty_install_dir "$install_dir"
  tmp_parent="$(mktemp -d)"
  tmp_checkout="$tmp_parent/exo"
  echo "Fetching Exo into $install_dir (staged via a temporary clone)..."
  git clone --branch "$REPO_REF" "$REPO_URL" "$tmp_checkout"
  shopt -s nullglob dotglob
  for entry in "$tmp_checkout"/*; do
    local name
    name="$(basename "$entry")"
    rm -rf "$install_dir/$name"
    mv "$entry" "$install_dir/"
  done
  rmdir "$tmp_checkout" "$tmp_parent"
}

main() {
  SETUP_ARGS=("$@")
  parse_args "$@"

  echo "Exo canonical setup"
  echo "This will install Exo into the current directory, write local keys to .env, and start Exo."
  echo "Repository: $REPO_URL"
  echo "Git ref: $REPO_REF"

  check_dependencies
  ensure_docker_running

  local install_dir launch_dir="$PWD"
  install_dir="$(choose_install_dir)"
  clone_or_reuse_repo "$install_dir"
  cd "$install_dir"
  trust_mise_config "$install_dir/mise.toml"
  ensure_toolchains

  # Build before the interactive prompts so answering them is the last step
  # before launch.
  info "Build Exo (takes a few minutes on first install)"
  ./exo.sh build

  local env_file="$install_dir/.env"
  if [[ ! -f "$env_file" && -f "$install_dir/.env.example" ]]; then
    cp "$install_dir/.env.example" "$env_file"
    chmod 600 "$env_file"
  else
    touch "$env_file"
    chmod 600 "$env_file"
  fi

  info "Configure model provider"
  if [[ -z "$MODEL_PROVIDER" ]]; then
    MODEL_PROVIDER="$(choose_model_provider)"
  fi
  configure_model_provider "$MODEL_PROVIDER"
  echo "Using $MODEL_PROVIDER_LABEL."

  if [[ "$MODEL_PROVIDER" == "openai-chatgpt" ]]; then
    info "Log in to ChatGPT"
    ./exo.sh auth login openai-chatgpt
  fi

  if [[ -z "$UPSTREAM_MODEL" ]]; then
    if [[ "$MODEL_PROVIDER" == "openrouter" ]]; then
      UPSTREAM_MODEL="$(prompt_text "OpenRouter model id" \
        "$DEFAULT_UPSTREAM_MODEL")"
    else
      UPSTREAM_MODEL="$DEFAULT_UPSTREAM_MODEL"
    fi
  fi

  if [[ "$MODEL_PROVIDER" != "openai-chatgpt" ]]; then
    info "Configure API keys"
    prompt_env_secret "$MODEL_API_KEY_ENV" "$env_file" \
      "$MODEL_PROVIDER_LABEL API key" true
  fi
  set_env_default "EXO_CHAT_BASE_URL" "${EXO_CHAT_BASE_URL:-$DEFAULT_EXO_CHAT_BASE_URL}" "$env_file"
  echo "Canonical setup uses ExoChat as the default external adapter and will show a browser URL to open."

  info "Configure Exo"
  USER_NAME="$(prompt_text "Your name, or blank to skip" "$USER_NAME")"
  AGENT_NAME="$(prompt_text "Agent display name" "$AGENT_NAME")"
  ./exo.sh write-profile ${USER_NAME:+--user-name "$USER_NAME"}

  if [[ "$MODEL_PROVIDER" == "openai-chatgpt" ]]; then
    info "Register model"
    ./exo.sh register-model --model "$MODEL_NAME" \
      --upstream-model "$UPSTREAM_MODEL" --secret-name openai-chatgpt \
      --provider openai-chatgpt
  else
    info "Store secret and register model"
    ./exo.sh register-model --model "$MODEL_NAME" \
      --upstream-model "$UPSTREAM_MODEL" --provider "$MODEL_PROVIDER" \
      --secret-name "$MODEL_PROVIDER" --secret-env "$MODEL_API_KEY_ENV" \
      ${MODEL_BASE_URL:+--base-url "$MODEL_BASE_URL"}
  fi

  info "Create your agent"
  ./exo.sh setup-agent --agent-name "$AGENT_NAME"

  print_success_banner "$install_dir" "$launch_dir"
}

print_success_banner() {
  local dir="$1"
  local launch_dir="$2"
  cat <<'EOF'

        \ \     / /
         \ \   / /
          \ \ / /
           > X <
          / / \ \
         / /   \ \
        /_/     \_\

     ___  __  __  ___
    / _ \ \ \/ / / _ \
   |  __/  >  < | (_) |
    \___| /_/\_\ \___/

EOF
  echo "Exo is installed and your agent is ready."
  echo
  echo "Your agent is long-running: once started, it stays up after you /exit the"
  echo "chat, and it serves an ExoChat URL you can open from any browser, anywhere."
  echo "If you ever lose that URL, ask your agent for it from the CLI chat."
  echo
  echo "Start chatting:"
  if [[ "$launch_dir" != "$dir" ]]; then
    echo "  cd $dir"
  fi
  echo "  ./exo.sh"
  echo
  echo "That same command starts or reconnects to your agent any time. Also useful:"
  echo "  ./exo.sh stop-all   stop everything; state is preserved"
  echo "  ./exo.sh fresh      wipe agents and start over"
  echo "  ./exo.sh --help     all commands"
}

main "$@"
