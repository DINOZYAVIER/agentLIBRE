#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/agentlibre-daemon-systemd-service.sh [OPTIONS]

Installs a user-systemd service for `agl serve`.

Options:
  --unit NAME           systemd user unit name
  --cwd PATH            working directory for the service
  --binary PATH         agl binary path
  --config PATH         local inference config TOML path
  --socket PATH         daemon Unix socket path
  --workspace-root PATH workspace root passed to agl serve
  --max-output-tokens N max generated tokens per turn
  --tool-mode MODE      read-only or write
  --log-filter FILTER   tracing filter for AGL_LOG
  --enable              enable the unit
  --restart             restart the unit after writing it
  --dry-run             print the unit without writing it
  -h, --help            show this help

Defaults:
  --unit              agl.service
  --cwd               current git repo root, or current directory outside git
  --binary            ./target/release/agl under the repo root
  --config            ~/.config/agentLIBRE/inference/local.toml
  --socket            ~/.local/state/agentLIBRE/daemon/agl.sock
  --workspace-root    repo root
  --max-output-tokens 256
  --tool-mode         read-only
  --log-filter        agentlibre=info,warn
EOF
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
# shellcheck source=systemd-lib.sh
source "$script_dir/systemd-lib.sh"
config_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"
state_home="${XDG_STATE_HOME:-$HOME/.local/state}"

unit="agl.service"
cwd="$(git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null || printf '%s' "$repo_root")"
binary="${AGL_DAEMON_BINARY:-$repo_root/target/release/agl}"
config="${AGL_DAEMON_CONFIG:-$config_home/agentLIBRE/inference/local.toml}"
socket="${AGL_DAEMON_SOCKET:-$state_home/agentLIBRE/daemon/agl.sock}"
workspace_root="${AGL_DAEMON_WORKSPACE_ROOT:-$cwd}"
max_output_tokens="${AGL_DAEMON_MAX_OUTPUT_TOKENS:-256}"
tool_mode="${AGL_DAEMON_TOOL_MODE:-read-only}"
log_filter="${AGL_LOG:-agentlibre=info,warn}"
enable=0
restart=0
dry_run=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --unit)
      unit="${2:?missing value for --unit}"
      shift 2
      ;;
    --cwd)
      cwd="${2:?missing value for --cwd}"
      shift 2
      ;;
    --binary)
      binary="${2:?missing value for --binary}"
      shift 2
      ;;
    --config)
      config="${2:?missing value for --config}"
      shift 2
      ;;
    --socket)
      socket="${2:?missing value for --socket}"
      shift 2
      ;;
    --workspace-root)
      workspace_root="${2:?missing value for --workspace-root}"
      shift 2
      ;;
    --max-output-tokens)
      max_output_tokens="${2:?missing value for --max-output-tokens}"
      shift 2
      ;;
    --tool-mode)
      tool_mode="${2:?missing value for --tool-mode}"
      shift 2
      ;;
    --log-filter)
      log_filter="${2:?missing value for --log-filter}"
      shift 2
      ;;
    --enable)
      enable=1
      shift
      ;;
    --restart)
      restart=1
      shift
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

agl_systemd_validate_unit_name "$unit"

for value_name in cwd binary config socket workspace_root; do
  value="${!value_name}"
  agl_systemd_validate_absolute_path "--${value_name//_/-}" "$value"
done

if [[ ! "$max_output_tokens" =~ ^[1-9][0-9]*$ ]]; then
  echo "--max-output-tokens must be a positive integer: $max_output_tokens" >&2
  exit 2
fi

case "$tool_mode" in
  read-only|write) ;;
  *)
    echo "--tool-mode must be read-only or write: $tool_mode" >&2
    exit 2
    ;;
esac

agl_systemd_validate_nonempty_no_newline "--log-filter" "$log_filter"
agl_systemd_require_dir "$dry_run" "$cwd" "working directory"
agl_systemd_require_dir "$dry_run" "$workspace_root" "workspace root"
agl_systemd_require_executable "$dry_run" "$binary"
agl_systemd_require_file "$dry_run" "$config" "config file"

unit_dir="$config_home/systemd/user"
unit_file="$unit_dir/$unit"
unit_content="[Unit]
Description=agentLIBRE daemon

[Service]
Type=simple
WorkingDirectory=$cwd
Environment=AGL_LOG=$log_filter
Environment=AGL_LOG_STDERR=always
ExecStart=$(agl_systemd_quote "$binary") serve --config $(agl_systemd_quote "$config") --socket $(agl_systemd_quote "$socket") --workspace-root $(agl_systemd_quote "$workspace_root") --max-output-tokens $max_output_tokens --tool-mode $tool_mode
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"

echo "unit: $unit"
echo "cwd: $cwd"
echo "binary: $binary"
echo "config: $config"
echo "socket: $socket"
echo "workspace root: $workspace_root"
echo "max output tokens: $max_output_tokens"
echo "tool mode: $tool_mode"
echo "log filter: $log_filter"
echo "unit file: $unit_file"

if [[ "$dry_run" -eq 1 ]]; then
  printf '\n%s' "$unit_content"
  exit 0
fi

agl_systemd_install_user_unit "$unit_dir" "$unit" "$unit_content" "$enable" "$restart"
