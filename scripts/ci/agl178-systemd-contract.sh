#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"
ci_cd_repo

temporary_root="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$temporary_root" 2>/dev/null || true
  rm -rf -- "$temporary_root"
}
trap cleanup EXIT

runtime_root="$temporary_root/runtime root %25"
generation="$runtime_root/libexec/agentlibre/generations/generation-test"
mkdir -p "$runtime_root/bin" "$generation" "$temporary_root/workspace" \
  "$temporary_root/state"
printf '#!/bin/sh\nexit 0\n' >"$generation/agl"
printf '#!/bin/sh\nexit 0\n' >"$generation/llama-server"
printf 'fixture\n' >"$generation/libllama-server-impl.so"
printf '{}\n' >"$generation/runtime-manifest.json"
chmod 0555 "$generation/agl" "$generation/llama-server" \
  "$generation/libllama-server-impl.so"
chmod 0444 "$generation/runtime-manifest.json"
chmod 0555 "$generation"
ln -s generations/generation-test "$runtime_root/libexec/agentlibre/current"
ln -s ../libexec/agentlibre/current/agl "$runtime_root/bin/agl"

socket="$temporary_root/state/agl.sock"
output="$(
  HOME="$temporary_root/home" \
  XDG_CONFIG_HOME="$temporary_root/config" \
  XDG_STATE_HOME="$temporary_root/state" \
    scripts/agentlibre-daemon-systemd-service.sh \
      --dry-run \
      --binary "$runtime_root/bin/agl" \
      --cwd "$temporary_root/workspace" \
      --workspace-root "$temporary_root/workspace" \
      --function service-test \
      --socket "$socket" \
      --max-output-tokens 64 \
      --tool-mode execute
)"

[[ "$output" == *"ExecStart=\"$generation/agl\" serve --systemd-activation"* ]] ||
  ci_fail "daemon unit does not execute the immutable agent generation: $output"
[[ "$output" != *"ExecStart=\"$runtime_root/bin/agl\""* ]] ||
  ci_fail "daemon unit still executes the mutable public link: $output"
[[ "$output" == *"Requires=agentlibre-daemon.socket agl-terminald.service"* ]] ||
  ci_fail "daemon unit omitted terminal service requirement: $output"
[[ "$output" == *"After=agentlibre-daemon.socket agl-terminald.service"* ]] ||
  ci_fail "daemon unit omitted terminal readiness ordering: $output"

printf 'agl178-systemd: passed\n'
