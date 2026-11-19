#!/usr/bin/env bash
# agl-chat (AGL-073): agentLIBRE's chat for the directory you are in.
#
#   agl-chat [--resume <id|name|last>] [--plane <name>] [--function <name>]
#   agl-chat --stop
#
# Builds the new agl and the chat when they are out of date, starts the
# daemon when none answers (the installed agl has no daemon), and opens
# the window. --stop stops the daemon.
set -euo pipefail

AGL=~/repos/agentLIBRE
CHAT=$AGL/crates/agentlibre-chat
KATARSA=~/repos/katarsa-shell
LOG=${XDG_STATE_HOME:-$HOME/.local/state}/agentLIBRE-chat
ORIGIN=$PWD

if [ "${1:-}" = "--stop" ]; then
  for pid in $(pgrep -f 'agl daemon' || true); do
    kill "$pid" 2>/dev/null || true
  done
  echo "the daemon is stopped"
  exit 0
fi

# 1. The new agl, from the checkout (the installed one has no daemon).
(cd "$AGL" && nix develop --command cargo build -q)

# 2. A daemon answering on its socket; one that answers already is kept.
mkdir -p "$LOG"
(
  cd "$AGL"
  exec setsid nix develop --command ./target/debug/agl daemon >>"$LOG/daemon.log" 2>&1 </dev/null
) &
for _ in $(seq 1 100); do
  if python3 -c "import socket, os; s = socket.socket(socket.AF_UNIX); s.settimeout(0.2); s.connect(os.environ['XDG_RUNTIME_DIR'] + '/agentLIBRE/agl.sock')" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

# 3. The chat, in katarsa-shell's shell (its Wayland and keyboard
#    libraries); the window opens a session in $ORIGIN.
args=("$@")
exec nix-shell "$KATARSA/shell.nix" \
  --run "(cd $CHAT && cargo build -q) && exec $CHAT/target/debug/agl-chat ${args[*]@Q}"
