#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/agentlibre-execd-systemd-service.sh [options]

Installs the user-systemd socket and service for `agl-execd`.

Options:
  --unit NAME          service unit name
  --binary PATH        agl-execd executable
  --socket PATH        execution Unix socket
  --log-filter FILTER  RUST_LOG value
  --enable             enable the socket
  --restart            restart the socket after writing units
  --dry-run            print units without writing them
  -h, --help           show this help
EOF
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=systemd-lib.sh
source "$script_dir/systemd-lib.sh"

config_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
state_home="${XDG_STATE_HOME:-$HOME/.local/state}"
unit="agentlibre-execd.service"
binary="${AGL_EXECD_BINARY:-$(command -v agl-execd || true)}"
socket="${AGL_EXECD_SOCKET:-$state_home/agentLIBRE/execd/execd.sock}"
log_filter="${RUST_LOG:-info}"
enable=0
restart=0
dry_run=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --unit) unit="${2:?missing value for --unit}"; shift 2 ;;
    --binary) binary="${2:?missing value for --binary}"; shift 2 ;;
    --socket) socket="${2:?missing value for --socket}"; shift 2 ;;
    --log-filter) log_filter="${2:?missing value for --log-filter}"; shift 2 ;;
    --enable) enable=1; shift ;;
    --restart) restart=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$binary" ]] || { echo "agl-execd is not installed; pass --binary" >&2; exit 1; }
binary="$(realpath -m -s -- "$binary")"
agl_systemd_validate_unit_name "$unit"
[[ "$unit" == *.service ]] || { echo "--unit must end in .service" >&2; exit 2; }
agl_systemd_validate_absolute_vars binary socket config_home data_home state_home
agl_systemd_validate_nonempty_no_newline "--log-filter" "$log_filter"
agl_systemd_require_executable "$dry_run" "$binary"
agl_systemd_prepare_private_socket_parent "$dry_run" "$socket"

unit_dir="$config_home/systemd/user"
socket_unit="${unit%.service}.socket"
service_content="[Unit]
Description=agentLIBRE execution service
Requires=$socket_unit
After=$socket_unit

[Service]
Type=simple
UMask=0077
Environment=$(agl_systemd_quote "RUST_LOG=$log_filter")
Environment=$(agl_systemd_quote "XDG_DATA_HOME=$data_home")
Environment=$(agl_systemd_quote "XDG_STATE_HOME=$state_home")
ExecStart=$(agl_systemd_quote "$binary")
StandardOutput=journal
StandardError=journal
SyslogIdentifier=agl-execd
Restart=on-failure
RestartSec=2
"
socket_content="[Unit]
Description=agentLIBRE execution socket

[Socket]
ListenStream=$(agl_systemd_escape_scalar "$socket")
FileDescriptorName=agentlibre-execution
SocketMode=0600
DirectoryMode=0700
RemoveOnStop=true
Accept=no
Service=$unit

[Install]
WantedBy=sockets.target
"

echo "service unit: $unit"
echo "socket unit: $socket_unit"
echo "binary: $binary"
echo "socket: $socket"

agl_systemd_print_or_install_user_unit "$dry_run" "$unit_dir" "$unit" "$service_content" 0 0
agl_systemd_print_or_install_user_unit "$dry_run" "$unit_dir" "$socket_unit" "$socket_content" "$enable" "$restart"
