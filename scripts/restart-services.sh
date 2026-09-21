#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/restart-services.sh [--stop|--restart] [--dry-run]

Controls the user-systemd services used by the local agentLIBRE deployment:
  agentlibre-execd.service/socket
  agentlibre-daemon.service/socket
  agl-matrix-bridge.service

Default action is --restart. --stop also stops both socket units so a later
client cannot activate a service that was meant to be offline.
EOF
}

action=restart
dry_run=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --stop) action=stop; shift ;;
    --restart) action=restart; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  if [[ "$dry_run" -eq 0 ]]; then
    "$@"
  fi
}

execd_service=agentlibre-execd.service
execd_socket=agentlibre-execd.socket
daemon_service=agentlibre-daemon.service
daemon_socket=agentlibre-daemon.socket
matrix_service=agl-matrix-bridge.service

run systemctl --user stop "$matrix_service" "$daemon_service" "$execd_service" \
  "$daemon_socket" "$execd_socket"

if [[ "$action" == restart ]]; then
  run systemctl --user start "$execd_socket" "$daemon_socket"
  run systemctl --user start "$execd_service" "$daemon_service" "$matrix_service"
fi

if [[ "$dry_run" -eq 0 ]]; then
  systemctl --user --no-pager --full status \
    "$execd_service" "$daemon_service" "$matrix_service"
fi
