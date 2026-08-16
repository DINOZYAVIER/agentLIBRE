#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_cd_repo

tmp_dir="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$tmp_dir" 2>/dev/null || true
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

runtime_root="$tmp_dir/runtime"
generation="$runtime_root/libexec/agentlibre/generations/generation-test"
mkdir -p "$runtime_root/bin" "$generation" "$tmp_dir/workspace" "$tmp_dir/state"
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

socket="$tmp_dir/state/agl.sock"
output="$(
  HOME="$tmp_dir/home" \
  XDG_CONFIG_HOME="$tmp_dir/config" \
  XDG_STATE_HOME="$tmp_dir/state" \
    scripts/agentlibre-daemon-systemd-service.sh \
      --dry-run \
      --binary "$runtime_root/bin/agl" \
      --cwd "$tmp_dir/workspace" \
      --workspace-root "$tmp_dir/workspace" \
      --function service-test \
      --socket "$socket" \
      --max-output-tokens 64 \
      --tool-mode execute
)"

[[ "$output" == *"resolved binary: $generation/agl"* ]] ||
  ci_fail "daemon unit did not resolve the sealed generation: $output"
[[ "$output" == *"private llama-server: $generation/llama-server"* ]] ||
  ci_fail "daemon unit omitted the private engine: $output"
[[ "$output" == *"runtime manifest: $generation/runtime-manifest.json"* ]] ||
  ci_fail "daemon unit omitted the sealed manifest: $output"
[[ "$output" == *"ListenStream=$socket"* && "$output" == *"Accept=no"* ]] ||
  ci_fail "daemon socket unit is not an exact private non-accepting socket: $output"
[[ "$output" == *"ExecStart=\"$generation/agl\" serve --systemd-activation"* ]] ||
  ci_fail "daemon service does not execute the validated sealed generation: $output"
[[ "$output" == *'--function "service-test"'* && "$output" != *"ExecStart="*" --config "* ]] ||
  ci_fail "package-bound function did not suppress raw inference config: $output"

expect_rejection() {
  local label="$1"
  shift
  local status=0
  HOME="$tmp_dir/home" XDG_CONFIG_HOME="$tmp_dir/config" XDG_STATE_HOME="$tmp_dir/state" \
    scripts/agentlibre-daemon-systemd-service.sh --dry-run \
      --binary "$runtime_root/bin/agl" --cwd "$tmp_dir/workspace" \
      --workspace-root "$tmp_dir/workspace" --function service-test \
      --socket "$socket" "$@" >/dev/null 2>&1 || status=$?
  [[ "$status" -ne 0 ]] || ci_fail "expected systemd rejection: $label"
}

chmod 0755 "$generation/llama-server"
expect_rejection "mutable engine"
chmod 0555 "$generation/llama-server"

printf '#!/bin/sh\nexit 0\n' >"$runtime_root/bin/agl-inference-worker"
chmod 0755 "$runtime_root/bin/agl-inference-worker"
expect_rejection "public legacy worker"

echo "systemd dry-run tests passed"
