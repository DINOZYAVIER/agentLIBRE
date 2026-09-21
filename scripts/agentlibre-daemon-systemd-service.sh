#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/agentlibre-daemon-systemd-service.sh [options]

Installs a user-systemd socket and service for `agl serve`.

Options:
  --unit NAME          service unit name
  --cwd PATH           service working directory
  --binary PATH        agl executable
  --socket PATH        daemon Unix socket
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
unit="agentlibre-daemon.service"
cwd="${HOME:?HOME is required}"
binary="${AGL_DAEMON_BINARY:-$(command -v agl || true)}"
socket="${AGL_DAEMON_SOCKET:-$state_home/agentLIBRE/daemon/agl.sock}"
config="$config_home/agentLIBRE/agentLIBRE.toml"
log_filter="${RUST_LOG:-info,agl_inference_engine=debug}"
enable=0
restart=0
dry_run=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --unit) unit="${2:?missing value for --unit}"; shift 2 ;;
    --cwd) cwd="${2:?missing value for --cwd}"; shift 2 ;;
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

[[ -n "$binary" ]] || { echo "agl is not installed; pass --binary" >&2; exit 1; }
binary="$(realpath -m -s -- "$binary")"
agl_systemd_validate_unit_name "$unit"
[[ "$unit" == *.service ]] || { echo "--unit must end in .service" >&2; exit 2; }
agl_systemd_validate_absolute_vars cwd binary config socket config_home data_home state_home
agl_systemd_validate_nonempty_no_newline "--log-filter" "$log_filter"
agl_systemd_require_dir "$dry_run" "$cwd" "working directory"
agl_systemd_require_executable "$dry_run" "$binary"
agl_systemd_require_file "$dry_run" "$config" "daemon config"
agl_systemd_prepare_private_socket_parent "$dry_run" "$socket"

unit_dir="$config_home/systemd/user"
socket_unit="${unit%.service}.socket"
service_content="[Unit]
Description=agentLIBRE daemon
Requires=$socket_unit
Wants=agentlibre-execd.socket
After=$socket_unit agentlibre-execd.socket

[Service]
Type=simple
UMask=0077
WorkingDirectory=$(agl_systemd_escape_scalar "$cwd")
Environment=$(agl_systemd_quote "RUST_LOG=$log_filter")
Environment=$(agl_systemd_quote "XDG_CONFIG_HOME=$config_home")
Environment=$(agl_systemd_quote "XDG_DATA_HOME=$data_home")
Environment=$(agl_systemd_quote "XDG_STATE_HOME=$state_home")
ExecStart=$(agl_systemd_quote "$binary") serve
StandardOutput=journal
StandardError=journal
SyslogIdentifier=agl
Restart=on-failure
RestartPreventExitStatus=78
RestartSec=5
"
socket_content="[Unit]
Description=agentLIBRE daemon socket

[Socket]
ListenStream=$(agl_systemd_escape_scalar "$socket")
FileDescriptorName=agentlibre
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
echo "human daemon config: $config"

agl_systemd_print_or_install_user_unit "$dry_run" "$unit_dir" "$unit" "$service_content" 0 0
agl_systemd_print_or_install_user_unit "$dry_run" "$unit_dir" "$socket_unit" "$socket_content" "$enable" "$restart"
