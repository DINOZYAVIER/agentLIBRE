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

require_output_not_contains() {
  local output="$1"
  local needle="$2"
  if [[ "$output" == *"$needle"* ]]; then
    printf 'expected dry-run output not to contain:\n%s\n\nactual output:\n%s\n' "$needle" "$output" >&2
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

require_output_contains "$daemon_output" "service unit: agl-test.service"
require_output_contains "$daemon_output" "socket unit: agl-test.socket"
require_output_contains "$daemon_output" "unit file: ${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}/systemd/user/agl-test.service"
require_output_contains "$daemon_output" "socket unit file: ${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}/systemd/user/agl-test.socket"
require_output_contains "$daemon_output" "WorkingDirectory=$tmp_dir/workspace"
require_output_contains "$daemon_output" "Environment=AGL_LOG=agentlibre=debug"
require_output_contains "$daemon_output" "Environment=AGL_LOG_STDERR=always"
require_output_contains "$daemon_output" "Requires=agl-test.socket"
require_output_contains "$daemon_output" "ExecStart=\"$tmp_dir/bin/agl\" serve --systemd-activation --config \"$tmp_dir/config/local.toml\" --workspace-root \"$tmp_dir/workspace\" --max-output-tokens 512 --tool-mode write"
require_output_contains "$daemon_output" "ListenStream=$tmp_dir/state/daemon/agl.sock"
require_output_contains "$daemon_output" "FileDescriptorName=agentlibre"
require_output_contains "$daemon_output" "SocketMode=0600"
require_output_contains "$daemon_output" "DirectoryMode=0700"
require_output_contains "$daemon_output" "RemoveOnStop=true"
require_output_contains "$daemon_output" "Accept=no"
require_output_contains "$daemon_output" "Service=agl-test.service"
require_output_contains "$daemon_output" "WantedBy=sockets.target"
if [[ -e "$tmp_dir/state" || -L "$tmp_dir/state" ]]; then
  ci_fail "daemon dry-run created or replaced the socket parent"
fi

bridge_output="$("$AGL_CI_REPO_ROOT/scripts/agentlibre-matrix-bridge-systemd-service.sh" \
  --dry-run \
  --unit agl-matrix-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$tmp_dir/bin/agl-matrix-bridge" \
  --config "$tmp_dir/config/matrix-bridge.toml" \
  --log-filter "agl_matrix_bridge=debug")"

require_output_contains "$bridge_output" "unit: agl-matrix-test.service"
require_output_contains "$bridge_output" "Wants=agentlibre-daemon.socket"
require_output_contains "$bridge_output" "After=agentlibre-daemon.socket"
require_output_not_contains "$bridge_output" "agl.service"
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

install_root="$tmp_dir/daemon-install"
mkdir -p \
  "$install_root/bin" \
  "$install_root/config" \
  "$install_root/fake-bin" \
  "$install_root/state/daemon" \
  "$install_root/workspace"
printf '#!/usr/bin/env bash\nexit 0\n' >"$install_root/bin/agl"
chmod 0755 "$install_root/bin/agl"
printf '[backend]\nkind = "llama_cpp"\n' >"$install_root/config/local.toml"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >>"${AGL_TEST_SYSTEMCTL_LOG:?}"\n' \
  >"$install_root/fake-bin/systemctl"
chmod 0755 "$install_root/fake-bin/systemctl"
chmod 0755 "$install_root/state" "$install_root/state/daemon"

env \
  HOME="$install_root/home" \
  XDG_CONFIG_HOME="$install_root/config-home" \
  AGL_TEST_SYSTEMCTL_LOG="$install_root/systemctl.log" \
  PATH="$install_root/fake-bin:$PATH" \
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
    --unit agl-install-test.service \
    --cwd "$install_root/workspace" \
    --binary "$install_root/bin/agl" \
    --config "$install_root/config/local.toml" \
    --socket "$install_root/state/daemon/agl.sock" \
    --workspace-root "$install_root/workspace" \
    >"$install_root/install.out"

[[ ! -L "$install_root/state/daemon" ]] ||
  ci_fail "daemon installer accepted a symlink socket parent"
[[ "$(stat -c '%u' -- "$install_root/state/daemon")" == "$(id -u)" ]] ||
  ci_fail "daemon installer did not preserve exact socket-parent ownership"
[[ "$(stat -c '%a' -- "$install_root/state/daemon")" == "700" ]] ||
  ci_fail "daemon installer did not tighten an existing socket parent to 0700"
[[ "$(stat -c '%a' -- "$install_root/state")" == "755" ]] ||
  ci_fail "daemon installer changed an ancestor instead of only the socket parent"
[[ -f "$install_root/config-home/systemd/user/agl-install-test.service" ]] ||
  ci_fail "daemon service unit was not installed in the temporary config home"
[[ -f "$install_root/config-home/systemd/user/agl-install-test.socket" ]] ||
  ci_fail "daemon socket unit was not installed in the temporary config home"

mkdir -p "$tmp_dir/symlink-target"
chmod 0755 "$tmp_dir/symlink-target"
ln -s "$tmp_dir/symlink-target" "$tmp_dir/symlink-parent"
symlink_status=0
"$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-symlink-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$tmp_dir/bin/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/symlink-parent/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/symlink.out" 2>"$tmp_dir/symlink.err" || symlink_status=$?
[[ "$symlink_status" -eq 1 ]] ||
  ci_fail "daemon dry-run did not reject a symlinked socket parent"
grep -F -- "must be canonical and contain no symlink components" "$tmp_dir/symlink.err" >/dev/null ||
  ci_fail "symlinked socket-parent error message changed"
[[ "$(stat -c '%a' -- "$tmp_dir/symlink-target")" == "755" ]] ||
  ci_fail "daemon dry-run mutated the symlink target"

printf 'systemd dry-run checks passed\n'
