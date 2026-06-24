#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

config="${1:-${AGL_MATRIX_SMOKE_CONFIG:-}}"
user_id="${2:-${AGL_MATRIX_SMOKE_USER_ID:-}}"
device_id="${3:-${AGL_MATRIX_SMOKE_DEVICE_ID:-}}"
timeout_seconds="${AGL_MATRIX_SMOKE_TIMEOUT_SECONDS:-300}"
bridge_bin="${AGL_MATRIX_SMOKE_BRIDGE_BIN:-$repo_root/target/debug/agl-matrix-bridge}"

[[ -n "$config" && -n "$user_id" && -n "$device_id" ]] || {
  echo "usage: scripts/agl-matrix-verify-device-smoke.sh /path/to/bridge.toml @user:server DEVICEID" >&2
  echo "or set AGL_MATRIX_SMOKE_CONFIG, AGL_MATRIX_SMOKE_USER_ID, and AGL_MATRIX_SMOKE_DEVICE_ID" >&2
  exit 2
}

cargo build -p agl-matrix-bridge >/dev/null

echo "This smoke is interactive. Confirm the SAS only if it matches on the other Matrix device." >&2
"$bridge_bin" verify-device \
  --config "$config" \
  --user-id "$user_id" \
  --device-id "$device_id" \
  --timeout-seconds "$timeout_seconds"
