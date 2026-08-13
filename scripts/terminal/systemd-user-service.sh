#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    'Usage: scripts/terminal/systemd-user-service.sh [--prefix PATH] [--apply] [--enable] [--restart]' \
    '' \
    'Validates one installed terminal generation and renders its user service/socket.' \
    'The default is a read-only dry run; --apply writes files atomically.'
}

fail() {
  printf 'agl-terminal-systemd: %s\n' "$*" >&2
  exit 1
}

prefix="${HOME:?HOME is required}/.local"
apply=0
enable=0
restart=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) prefix="${2:?--prefix requires a path}"; shift 2 ;;
    --apply) apply=1; shift ;;
    --enable) apply=1; enable=1; shift ;;
    --restart) apply=1; restart=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) fail "unknown option: $1" ;;
  esac
done

[[ "$prefix" == /* ]] || fail "prefix must be absolute: $prefix"
prefix="$(realpath -m -s -- "$prefix")"
current_link="$prefix/libexec/agl-terminal/current"
[[ -L "$current_link" ]] || fail "terminal current generation is not installed: $current_link"
generation="$(realpath -e -- "$current_link")" || fail "terminal current link is broken"
service="$generation/agl-terminald"
launcher="$generation/agl-process-launcher"
ui="$generation/agl-terminal"
manifest="$generation/runtime-manifest.json"
[[ "$(stat -c '%a' -- "$generation")" == 555 ]] ||
  fail "terminal generation directory is mutable: $generation"
[[ -x "$service" && ! -L "$service" && -x "$launcher" && ! -L "$launcher" &&
  -x "$ui" && ! -L "$ui" &&
  -f "$manifest" && ! -L "$manifest" ]] || fail "installed generation is incomplete: $generation"
manifest_digest="$(sha256sum -- "$manifest" | awk '{print $1}')"
[[ "$(basename -- "$generation")" == "generation-$manifest_digest" ]] ||
  fail "terminal generation directory does not match its full manifest digest"
for entry in "$service" "$launcher" "$ui"; do
  [[ "$(stat -c '%a:%h' -- "$entry")" == 555:1 ]] ||
    fail "installed generation entry is mutable or aliased: $entry"
done
[[ "$(stat -c '%a:%h' -- "$manifest")" == 444:1 ]] ||
  fail "installed generation manifest is mutable or aliased: $manifest"
[[ "$(find "$generation" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort)" == $'agl-process-launcher\nagl-terminal\nagl-terminald\nruntime-manifest.json' ]] ||
  fail "installed generation inventory is not canonical: $generation"
service_digest="sha256:$(sha256sum -- "$service" | awk '{print $1}')"
launcher_digest="sha256:$(sha256sum -- "$launcher" | awk '{print $1}')"
ui_digest="sha256:$(sha256sum -- "$ui" | awk '{print $1}')"
grep -F '"schema": "agl-terminal.runtime-generation.v2"' "$manifest" >/dev/null ||
  fail "terminal generation manifest is not v2"
grep -F "\"sha256\": \"$service_digest\"" "$manifest" >/dev/null ||
  fail "service bytes do not match the installed manifest"
grep -F "\"sha256\": \"$launcher_digest\"" "$manifest" >/dev/null ||
  fail "launcher bytes do not match the installed manifest"
grep -F "\"sha256\": \"$ui_digest\"" "$manifest" >/dev/null ||
  fail "UI bytes do not match the installed manifest"

config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
state_home="${XDG_STATE_HOME:-$HOME/.local/state}"
runtime_home="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
config_dir="$config_home/agl-terminal"
unit_dir="$config_home/systemd/user"
environment_file="$config_dir/service.env"
service_unit="$unit_dir/agl-terminald.service"
socket_unit="$unit_dir/agl-terminald.socket"
data_root="$data_home/agl-terminal"
state_root="$state_home/agl-terminal"
runtime_root="$runtime_home/agl-terminal"
socket="$runtime_root/terminal.sock"

for value in "$service" "$environment_file" "$data_root" "$state_root" "$runtime_root" "$socket"; do
  [[ "$value" != *$'\n'* && "$value" != *$'\r'* ]] || fail "paths must not contain newlines"
done

quote_env() {
  local value="${1//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

quote_unit() {
  local value="${1//\\/\\\\}"
  value="${value//\%/%%}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

escape_unit_scalar() {
  local value="$1"
  value="${value//\\/\\x5c}"
  value="${value// /\\x20}"
  value="${value//$'\t'/\\x09}"
  value="${value//\"/\\x22}"
  value="${value//\'/\\x27}"
  value="${value//\%/%%}"
  printf '%s' "$value"
}

environment_content="# Managed-by: agl-terminal generation installer v2
AGL_TERMINALD_SOCKET=$(quote_env "$socket")
AGL_TERMINALD_DATA_ROOT=$(quote_env "$data_root")
AGL_TERMINALD_STATE_ROOT=$(quote_env "$state_root")
AGL_TERMINALD_RUNTIME_ROOT=$(quote_env "$runtime_root")
"
service_content="# Managed-by: agl-terminal generation installer v2
[Unit]
Description=agentLIBRE terminal runtime
Requires=agl-terminald.socket
After=agl-terminald.socket
Before=agentlibre-daemon.service

[Service]
Type=notify
NotifyAccess=main
UMask=0077
EnvironmentFile=$(escape_unit_scalar "$environment_file")
Environment=AGL_TERMINALD_SYSTEMD_ACTIVATION=1
ExecStart=$(quote_unit "$service")
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=$(escape_unit_scalar "$data_root") $(escape_unit_scalar "$state_root") $(escape_unit_scalar "$runtime_root")
"
socket_content="# Managed-by: agl-terminal generation installer v2
[Unit]
Description=agentLIBRE terminal runtime socket
Before=agl-terminald.service agentlibre-daemon.service

[Socket]
ListenStream=$(escape_unit_scalar "$socket")
FileDescriptorName=agl-terminal
SocketMode=0600
DirectoryMode=0700
RemoveOnStop=true
Accept=no
Service=agl-terminald.service

[Install]
WantedBy=sockets.target
"

printf 'generation: %s\nservice: %s\nlauncher: %s\nbuild id: %s\n' \
  "$generation" "$service" "$launcher" "$service_digest"
printf 'environment file: %s\nservice unit: %s\nsocket unit: %s\n' \
  "$environment_file" "$service_unit" "$socket_unit"
printf '%s\n%s\n%s\n' "$environment_content" "$service_content" "$socket_content"

(( apply == 1 )) || { printf 'dry run; pass --apply to install units\n'; exit 0; }
managed_marker='# Managed-by: agl-terminal generation installer v2'
for owned_file in "$environment_file" "$service_unit" "$socket_unit"; do
  if [[ -e "$owned_file" || -L "$owned_file" ]]; then
    [[ -f "$owned_file" && ! -L "$owned_file" &&
      "$(stat -c '%u' -- "$owned_file")" == "$(id -u)" &&
      "$(head -n1 -- "$owned_file")" == "$managed_marker" ]] ||
      fail "refusing unmanaged terminal systemd surface: $owned_file"
  fi
done
for dropin_dir in "$unit_dir/agl-terminald.service.d" "$unit_dir/agl-terminald.socket.d"; do
  [[ ! -e "$dropin_dir" && ! -L "$dropin_dir" ]] ||
    fail "refusing terminal systemd drop-in surface: $dropin_dir"
done
mkdir -p -- "$config_dir" "$unit_dir" "$data_root" "$state_root" "$runtime_root"
chmod 0700 "$config_dir" "$state_root" "$runtime_root"
environment_tmp="$(mktemp "$config_dir/.service.env.XXXXXXXX")"
service_tmp="$(mktemp "$unit_dir/.agl-terminald.service.XXXXXXXX")"
socket_tmp="$(mktemp "$unit_dir/.agl-terminald.socket.XXXXXXXX")"
trap 'rm -f -- "$environment_tmp" "$service_tmp" "$socket_tmp"' EXIT
printf '%s' "$environment_content" >"$environment_tmp"
printf '%s' "$service_content" >"$service_tmp"
printf '%s' "$socket_content" >"$socket_tmp"
chmod 0600 "$environment_tmp"
chmod 0644 "$service_tmp" "$socket_tmp"
validation_dir="$(mktemp -d)"
trap 'rm -f -- "$environment_tmp" "$service_tmp" "$socket_tmp"; rm -rf -- "$validation_dir"' EXIT
cp -- "$service_tmp" "$validation_dir/agl-terminald.service"
cp -- "$socket_tmp" "$validation_dir/agl-terminald.socket"
systemd-analyze verify \
  "$validation_dir/agl-terminald.service" \
  "$validation_dir/agl-terminald.socket"
rm -rf -- "$validation_dir"
mv -fT -- "$environment_tmp" "$environment_file"
mv -fT -- "$service_tmp" "$service_unit"
mv -fT -- "$socket_tmp" "$socket_unit"
trap - EXIT
systemctl --user daemon-reload
(( enable == 0 )) || systemctl --user enable --now agl-terminald.socket
(( restart == 0 )) || systemctl --user restart agl-terminald.service
printf 'installed exact agl-terminald user units\n'
