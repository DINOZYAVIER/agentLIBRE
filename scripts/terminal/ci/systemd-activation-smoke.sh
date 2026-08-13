#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
temporary_root="$(mktemp -d)"
activation_pid=""
cleanup() {
  if [[ -n "$activation_pid" ]] && kill -0 "$activation_pid" 2>/dev/null; then
    kill -TERM "$activation_pid" 2>/dev/null || true
    wait "$activation_pid" 2>/dev/null || true
  fi
  chmod -R u+w "$temporary_root" 2>/dev/null || true
  rm -rf -- "$temporary_root"
}
trap cleanup EXIT

command -v systemd-socket-activate >/dev/null || {
  printf 'systemd-activation-smoke: skipped (systemd-socket-activate unavailable)\n'
  exit 0
}

prefix="$temporary_root/prefix"
scripts/terminal/install.sh --prefix "$prefix" --debug >/dev/null
generation="$(realpath -e -- "$prefix/libexec/agl-terminal/current")"
service="$generation/agl-terminald"
manifest="$generation/runtime-manifest.json"
manifest_digest="sha256:$(sha256sum -- "$manifest" | awk '{print $1}')"

socket="$temporary_root/runtime/terminal.sock"
runtime_root="$(dirname -- "$socket")"
state_root="$temporary_root/state"
data_root="$temporary_root/data"
mkdir -p "$(dirname -- "$socket")" "$state_root" "$data_root"

systemd-socket-activate \
  --now \
  --listen="$socket" \
  --fdname=agl-terminal \
  --setenv=AGL_TERMINALD_SYSTEMD_ACTIVATION=1 \
  --setenv="AGL_TERMINALD_SOCKET=$socket" \
  --setenv="AGL_TERMINALD_DATA_ROOT=$data_root" \
  --setenv="AGL_TERMINALD_STATE_ROOT=$state_root" \
  --setenv="AGL_TERMINALD_RUNTIME_ROOT=$runtime_root" \
  "$service" \
  >"$temporary_root/activation.log" 2>&1 &
activation_pid=$!

identity="$runtime_root/service-identity.json"
for _ in {1..500}; do
  [[ -f "$identity" ]] && break
  kill -0 "$activation_pid" 2>/dev/null || {
    cat "$temporary_root/activation.log" >&2
    printf 'systemd-activation-smoke: service exited before identity publication\n' >&2
    exit 1
  }
  sleep 0.02
done
[[ -f "$identity" ]] || {
  cat "$temporary_root/activation.log" >&2
  printf 'systemd-activation-smoke: identity was not published\n' >&2
  exit 1
}
grep -F "\"manifest_digest\":\"$manifest_digest\"" "$identity" >/dev/null || {
  printf 'systemd-activation-smoke: published identity has the wrong manifest digest\n' >&2
  exit 1
}
[[ -S "$socket" ]] || {
  printf 'systemd-activation-smoke: activated Unix socket is missing\n' >&2
  exit 1
}

kill -TERM "$activation_pid"
activation_status=0
wait "$activation_pid" || activation_status=$?
[[ "$activation_status" -eq 0 || "$activation_status" -eq 143 ]] || {
  printf 'systemd-activation-smoke: activation wrapper exited with %s\n' "$activation_status" >&2
  exit 1
}
activation_pid=""
printf 'systemd-activation-smoke: passed\n'
