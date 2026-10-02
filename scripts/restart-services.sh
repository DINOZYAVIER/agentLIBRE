#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/restart-services.sh [--stop|--restart] [--dry-run]

Stop/restart the user-systemd daemon and execution service including sockets.
Include the Matrix bridge only when its unit is installed. Restart reloads
unit files and clears failed/start-limit state before starting the services.
USAGE
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
  if [[ "$dry_run" -eq 0 ]]; then "$@"; fi
}
services=(agentlibre-daemon.service agentlibre-execd.service)
sockets=(agentlibre-daemon.socket agentlibre-execd.socket)
matrix=()
if [[ "$dry_run" -eq 1 ]] || [[ "$(systemctl --user show agl-matrix-bridge.service -p LoadState --value)" == loaded ]]; then
  matrix=(agl-matrix-bridge.service)
fi
installed=()
for unit in "${matrix[@]}" "${services[@]}" "${sockets[@]}"; do
  if [[ "$dry_run" -eq 1 ]] || [[ "$(systemctl --user show "$unit" -p LoadState --value)" == loaded ]]; then
    installed+=("$unit")
  fi
done
if [[ ${#installed[@]} -gt 0 ]]; then run systemctl --user stop "${installed[@]}"; fi
if [[ "$action" == restart ]]; then
  run systemctl --user daemon-reload
  run systemctl --user reset-failed "${services[@]}" "${sockets[@]}" "${matrix[@]}"
  run systemctl --user start agentlibre-execd.socket agentlibre-daemon.socket
  run systemctl --user start agentlibre-execd.service agentlibre-daemon.service
  if [[ ${#matrix[@]} -gt 0 ]]; then run systemctl --user start "${matrix[@]}"; fi
  if [[ "$dry_run" -eq 0 ]]; then
    systemctl --user is-active "${services[@]}" "${sockets[@]}" "${matrix[@]}"
  fi
elif [[ "$dry_run" -eq 0 ]]; then
  # Inactive is the successful result of --stop, not an is-active/status error.
  if [[ ${#installed[@]} -gt 0 ]]; then
    systemctl --user show "${installed[@]}" -p Id -p ActiveState
  fi
fi
