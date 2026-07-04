#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_cd_repo
ci_section "Systemd service dry-run"

tmp_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

require_output_contains() {
  local output="$1"
  local needle="$2"
  if [[ "$output" != *"$needle"* ]]; then
    printf 'expected dry-run output to contain:\n%s\n\nactual output:\n%s\n' "$needle" "$output" >&2
    exit 1
  fi
}

daemon_output="$("$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$tmp_dir/bin/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  --max-output-tokens 512 \
  --tool-mode write \
  --log-filter "agentlibre=debug")"

require_output_contains "$daemon_output" "unit: agl-test.service"
require_output_contains "$daemon_output" "unit file: ${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}/systemd/user/agl-test.service"
require_output_contains "$daemon_output" "WorkingDirectory=$tmp_dir/workspace"
require_output_contains "$daemon_output" "Environment=AGL_LOG=agentlibre=debug"
require_output_contains "$daemon_output" "Environment=AGL_LOG_STDERR=always"
require_output_contains "$daemon_output" "ExecStart=\"$tmp_dir/bin/agl\" serve --config \"$tmp_dir/config/local.toml\" --socket \"$tmp_dir/state/daemon/agl.sock\" --workspace-root \"$tmp_dir/workspace\" --max-output-tokens 512 --tool-mode write"

bridge_output="$("$AGL_CI_REPO_ROOT/scripts/agentlibre-matrix-bridge-systemd-service.sh" \
  --dry-run \
  --unit agl-matrix-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$tmp_dir/bin/agl-matrix-bridge" \
  --config "$tmp_dir/config/matrix-bridge.toml" \
  --log-filter "agl_matrix_bridge=debug")"

require_output_contains "$bridge_output" "unit: agl-matrix-test.service"
require_output_contains "$bridge_output" "WorkingDirectory=$tmp_dir/workspace"
require_output_contains "$bridge_output" "UMask=0077"
require_output_contains "$bridge_output" "Environment=AGL_MATRIX_LOG=agl_matrix_bridge=debug"
require_output_contains "$bridge_output" "ExecStart=\"$tmp_dir/bin/agl-matrix-bridge\" sync --config \"$tmp_dir/config/matrix-bridge.toml\""

invalid_status=0
"$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit ../bad.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$tmp_dir/bin/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/invalid-unit.out" 2>"$tmp_dir/invalid-unit.err" || invalid_status=$?

if [[ "$invalid_status" -ne 2 ]]; then
  printf 'expected invalid unit dry-run to exit 2, got %s\n' "$invalid_status" >&2
  exit 1
fi

grep -F -- "--unit must be a unit name" "$tmp_dir/invalid-unit.err" >/dev/null ||
  ci_fail "invalid unit error message changed"

printf 'systemd dry-run checks passed\n'
