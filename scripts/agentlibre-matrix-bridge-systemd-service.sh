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
  --cwd         current git repo root, or current directory outside git
  --binary      ./target/release/agl-matrix-bridge under the repo root
  --config      ~/.config/agentLIBRE/matrix-bridge/agl.toml
  --log-filter  agl_matrix_bridge=info,matrix_sdk=warn,warn
EOF
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
# shellcheck source=systemd-lib.sh
source "$script_dir/systemd-lib.sh"
config_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"

unit="agl-matrix-bridge.service"
cwd="$(git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null || printf '%s' "$repo_root")"
binary="${AGL_MATRIX_BRIDGE_BINARY:-$repo_root/target/release/agl-matrix-bridge}"
config="${AGL_MATRIX_BRIDGE_CONFIG:-$config_home/agentLIBRE/matrix-bridge/agl.toml}"
log_filter="${AGL_MATRIX_LOG:-agl_matrix_bridge=info,matrix_sdk=warn,warn}"
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

agl_systemd_validate_unit_name "$unit"
agl_systemd_validate_absolute_vars cwd binary config

agl_systemd_validate_nonempty_no_newline "--log-filter" "$log_filter"
agl_systemd_require_dir "$dry_run" "$cwd" "working directory"
agl_systemd_require_executable "$dry_run" "$binary"
agl_systemd_require_file "$dry_run" "$config" "config file"

unit_dir="$config_home/systemd/user"
unit_file="$unit_dir/$unit"
unit_content="[Unit]
Description=agentLIBRE Matrix bridge
Wants=agl.service
After=agl.service

[Service]
Type=simple
WorkingDirectory=$cwd
UMask=0077
Environment=AGL_MATRIX_LOG=$log_filter
ExecStart=$(agl_systemd_quote "$binary") sync --config $(agl_systemd_quote "$config")
Restart=always
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
