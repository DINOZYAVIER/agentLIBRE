#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/agentlibre-matrix-bridge-systemd-service.sh [OPTIONS]

Installs a user-systemd service for agl-matrix-bridge.

Options:
  --unit NAME          systemd user unit name
  --cwd PATH           working directory for the service
  --binary PATH        agl-matrix-bridge binary path
  --config PATH        bridge config TOML path
  --log-filter FILTER  tracing filter for AGL_MATRIX_LOG
  --enable             enable the unit
  --restart            restart the unit after writing it
  --dry-run            print the unit without writing it
  -h, --help           show this help

Defaults:
  --unit        agl-matrix-bridge.service
  --cwd         home directory
  --binary      installed agl-matrix-bridge from PATH, or AGL_MATRIX_BRIDGE_BINARY
  --config      ~/.config/agentLIBRE/matrix-bridge/agl.toml
  --log-filter  agl_matrix_bridge=info,matrix_sdk=warn,matrix_sdk::http_client=off,matrix_sdk_crypto::backups=error,warn
EOF
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=systemd-lib.sh
source "$script_dir/systemd-lib.sh"
config_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"

unit="agl-matrix-bridge.service"
cwd="${HOME:?HOME is required}"
binary="${AGL_MATRIX_BRIDGE_BINARY:-$(command -v agl-matrix-bridge || true)}"
config="${AGL_MATRIX_BRIDGE_CONFIG:-$config_home/agentLIBRE/matrix-bridge/agl.toml}"
log_filter="${AGL_MATRIX_LOG:-agl_matrix_bridge=info,matrix_sdk=warn,matrix_sdk::http_client=off,matrix_sdk_crypto::backups=error,warn}"
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

[[ -n "$binary" ]] || { echo "agl-matrix-bridge is not installed; pass --binary" >&2; exit 1; }
binary="$(realpath -m -s -- "$binary")"
agl_systemd_validate_unit_name "$unit"
agl_systemd_validate_absolute_vars cwd binary config

agl_systemd_validate_nonempty_no_newline "--log-filter" "$log_filter"
agl_systemd_require_dir "$dry_run" "$cwd" "working directory"
agl_systemd_require_executable "$dry_run" "$binary"
if [[ "$enable" == 1 || "$restart" == 1 ]]; then
  resolved_binary="$(realpath -e -- "$binary")"
  if git -C "$(dirname -- "$resolved_binary")" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "Matrix service binary is inside a Git worktree; install it outside the worktree before enabling: $resolved_binary" >&2
    exit 1
  fi
fi
agl_systemd_require_file "$dry_run" "$config" "config file"

unit_dir="$config_home/systemd/user"
unit_file="$unit_dir/$unit"
unit_content="[Unit]
Description=agentLIBRE Matrix bridge
Wants=agentlibre-daemon.socket
After=agentlibre-daemon.socket

[Service]
Type=simple
WorkingDirectory=$(agl_systemd_escape_scalar "$cwd")
UMask=0077
Environment=$(agl_systemd_quote "AGL_MATRIX_LOG=$log_filter")
ExecStart=$(agl_systemd_quote "$binary") sync --config $(agl_systemd_quote "$config")
StandardOutput=journal
StandardError=journal
SyslogIdentifier=agl-matrix-bridge
Restart=on-failure
RestartPreventExitStatus=78
RestartSec=5

[Install]
WantedBy=default.target
"

echo "unit: $unit"
echo "cwd: $cwd"
echo "binary: $binary"
echo "config: $config"
echo "log filter: $log_filter"
echo "unit file: $unit_file"

agl_systemd_print_or_install_user_unit \
  "$dry_run" \
  "$unit_dir" \
  "$unit" \
  "$unit_content" \
  "$enable" \
  "$restart"
