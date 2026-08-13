#!/usr/bin/env bash
set -euo pipefail

: "${AGL178_REAL_SYSTEMCTL:?missing real systemctl path}"
: "${AGL178_SYSTEMD_MANAGER_RUNTIME_DIR:?missing user manager runtime directory}"

exec env XDG_RUNTIME_DIR="$AGL178_SYSTEMD_MANAGER_RUNTIME_DIR" \
  "$AGL178_REAL_SYSTEMCTL" "$@"
