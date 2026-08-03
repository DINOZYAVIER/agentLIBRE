#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_cd_repo
ci_section "Systemd service dry-run"
ci_need_tool cc
ci_need_tool readelf
unset VK_DRIVER_FILES VK_ICD_FILENAMES

tmp_dir="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$tmp_dir" 2>/dev/null || true
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

fixture_bundle_digest="$(printf 'b%.0s' {1..64})"

create_worker_fixture() {
  local worker="$1"
  local source_file="$tmp_dir/worker-fixture.c"
  local generation
  local roots
  local reference
  local -a references=()
  printf '#include <stdlib.h>\nint main(void) { return 0; }\n' >"$source_file"
  cc "$source_file" \
    -Wl,-rpath,"\$ORIGIN/agl-inference-native/sha256-$fixture_bundle_digest" \
    -o "$worker"
  generation="$(dirname -- "$worker")"
  roots="$generation/.nix-gc-roots"
  mapfile -t references < <(
    {
      readelf -d "$worker"
      readelf -l "$worker"
    } | grep -oE '/nix/store/[0-9a-z]{32}-[A-Za-z0-9+._?=-]+' | sort -u || true
  )
  if (( ${#references[@]} > 0 )); then
    mkdir -p "$roots"
    for reference in "${references[@]}"; do
      ln -s "$reference" "$roots/$(basename -- "$reference")"
    done
    chmod 0555 "$roots"
  fi
}

create_native_bundle_fixture() {
  local generation="$1"
  local base="$generation/agl-inference-native"
  local bundle="$base/sha256-$fixture_bundle_digest"
  local library
  mkdir -p "$bundle"
  for library in \
    libllama-common.so.0 \
    libmtmd.so.0 \
    libllama.so.0 \
    libggml.so.0 \
    libggml-base.so.0 \
    libggml-cpu-test.so
  do
    : >"$bundle/$library"
    chmod 0555 "$bundle/$library"
  done
  chmod 0555 "$bundle"
  chmod 0555 "$base"
}

create_runtime_manifest_fixture() {
  local generation="$1"
  printf '{}\n' >"$generation/runtime-manifest.json"
  chmod 0444 "$generation/runtime-manifest.json"
}

runtime_root="$tmp_dir/runtime"
runtime_generation="$runtime_root/libexec/agentlibre/generations/generation-test"
mkdir -p "$tmp_dir/bin" "$runtime_root/bin" "$runtime_generation"
chmod 0755 \
  "$runtime_root" \
  "$runtime_root/bin" \
  "$runtime_root/libexec" \
  "$runtime_root/libexec/agentlibre" \
  "$runtime_root/libexec/agentlibre/generations"
printf '#!/usr/bin/env bash\nexit 0\n' >"$runtime_generation/agl"
printf '#!/usr/bin/env bash\nexit 0\n' >"$runtime_generation/agl-process-launcher"
create_worker_fixture "$runtime_generation/agl-inference-worker"
create_native_bundle_fixture "$runtime_generation"
create_runtime_manifest_fixture "$runtime_generation"
chmod 0555 \
  "$runtime_generation/agl" \
  "$runtime_generation/agl-process-launcher" \
  "$runtime_generation/agl-inference-worker"
chmod 0555 "$runtime_generation"
ln -s generations/generation-test "$runtime_root/libexec/agentlibre/current"
ln -s ../libexec/agentlibre/current/agl "$runtime_root/bin/agl"
ln -s ../libexec/agentlibre/current/agl-process-launcher \
  "$runtime_root/bin/agl-process-launcher"
ln -s "$runtime_root/bin/agl" "$tmp_dir/bin/agl"

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

daemon_output="$(env -u VK_DRIVER_FILES -u VK_ICD_FILENAMES PATH="$tmp_dir/bin:$PATH" \
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-test.service \
  --cwd "$tmp_dir/workspace" \
  --function "gemma4-31b-32k" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  --max-output-tokens 512 \
  --tool-mode execute \
  --log-filter "agentlibre=debug")"

require_output_contains "$daemon_output" "service unit: agl-test.service"
require_output_contains "$daemon_output" "socket unit: agl-test.socket"
require_output_contains "$daemon_output" "requested binary: $tmp_dir/bin/agl"
require_output_contains "$daemon_output" "binary: $runtime_root/bin/agl"
require_output_contains "$daemon_output" "resolved binary: $runtime_generation/agl"
require_output_contains "$daemon_output" "process launcher: $runtime_generation/agl-process-launcher"
require_output_contains "$daemon_output" "private inference worker: $runtime_generation/agl-inference-worker"
require_output_contains "$daemon_output" "native inference bundle: $runtime_generation/agl-inference-native"
require_output_contains "$daemon_output" "runtime manifest: $runtime_generation/runtime-manifest.json"
require_output_contains "$daemon_output" "unit file: ${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}/systemd/user/agl-test.service"
require_output_contains "$daemon_output" "socket unit file: ${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}/systemd/user/agl-test.socket"
require_output_contains "$daemon_output" "WorkingDirectory=$tmp_dir/workspace"
require_output_contains "$daemon_output" "Environment=AGL_LOG=agentlibre=debug"
require_output_contains "$daemon_output" "Environment=AGL_LOG_STDERR=always"
require_output_contains "$daemon_output" "Vulkan driver manifests: none (CPU-only discovery)"
require_output_not_contains "$daemon_output" "Environment=\"VK_DRIVER_FILES="
require_output_contains "$daemon_output" \
  "UnsetEnvironment=VK_DRIVER_FILES VK_ICD_FILENAMES"
require_output_contains "$daemon_output" "UMask=0077"
require_output_contains "$daemon_output" "Requires=agl-test.socket"
require_output_contains "$daemon_output" "function profile: gemma4-31b-32k (embedded; local inference config disabled)"
require_output_contains "$daemon_output" "ExecStart=\"$runtime_root/bin/agl\" serve --systemd-activation --workspace-root \"$tmp_dir/workspace\" --function \"gemma4-31b-32k\" --max-output-tokens 512 --tool-mode execute"
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

conflicting_profile_status=0
env -u VK_DRIVER_FILES -u VK_ICD_FILENAMES PATH="$tmp_dir/bin:$PATH" \
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-conflicting-profile.service \
  --cwd "$tmp_dir/workspace" \
  --config "$tmp_dir/config/local.toml" \
  --function gemma4-31b-32k \
  --socket "$tmp_dir/state/daemon/agl-conflicting.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/conflicting-profile.out" \
  2>"$tmp_dir/conflicting-profile.err" || conflicting_profile_status=$?
[[ "$conflicting_profile_status" -eq 2 ]] ||
  ci_fail "daemon installer accepted conflicting --function and --config"
rg -F -- "--function owns its inference profile" "$tmp_dir/conflicting-profile.err" >/dev/null ||
  ci_fail "daemon installer omitted the conflicting profile diagnostic"

vulkan_manifest="$tmp_dir/vulkan/%n/icd.d/driver.json"
fallback_manifest="$tmp_dir/vulkan/icd.d/fallback.json"
escaped_vulkan_manifest="${vulkan_manifest//%/%%}"
vulkan_output="$(env \
  VK_DRIVER_FILES="$vulkan_manifest" \
  VK_ICD_FILENAMES="$fallback_manifest" \
  PATH="$tmp_dir/bin:$PATH" \
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-vulkan-test.service \
  --cwd "$tmp_dir/workspace" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace")"
require_output_contains "$vulkan_output" \
  "Vulkan driver manifests: $vulkan_manifest (from VK_DRIVER_FILES)"
require_output_contains "$vulkan_output" \
  "Environment=\"VK_DRIVER_FILES=$escaped_vulkan_manifest\""
require_output_contains "$vulkan_output" \
  $'Environment="VK_DRIVER_FILES='"$escaped_vulkan_manifest"$'"\nUnsetEnvironment=VK_ICD_FILENAMES\nExecStart='
require_output_not_contains "$vulkan_output" "$fallback_manifest"

legacy_vulkan_output="$(env -u VK_DRIVER_FILES \
  VK_ICD_FILENAMES="$fallback_manifest" \
  PATH="$tmp_dir/bin:$PATH" \
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-vulkan-legacy-test.service \
  --cwd "$tmp_dir/workspace" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace")"
require_output_contains "$legacy_vulkan_output" \
  "Vulkan driver manifests: $fallback_manifest (from VK_ICD_FILENAMES)"
require_output_contains "$legacy_vulkan_output" \
  "Environment=\"VK_DRIVER_FILES=$fallback_manifest\""

empty_primary_status=0
env \
  VK_DRIVER_FILES= \
  VK_ICD_FILENAMES="$fallback_manifest" \
  PATH="$tmp_dir/bin:$PATH" \
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-vulkan-empty-test.service \
  --cwd "$tmp_dir/workspace" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/vulkan-empty.out" \
  2>"$tmp_dir/vulkan-empty.err" || empty_primary_status=$?
[[ "$empty_primary_status" -eq 2 ]] ||
  ci_fail "daemon installer did not reject an explicitly empty VK_DRIVER_FILES"
grep -F "VK_DRIVER_FILES must select nonempty colon-separated Vulkan manifest paths" \
  "$tmp_dir/vulkan-empty.err" >/dev/null ||
  ci_fail "empty VK_DRIVER_FILES rejection was not actionable"

expect_native_bundle_rejection() {
  local label="$1"
  local status=0
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
    --dry-run \
    --unit "agl-native-$label.service" \
    --cwd "$tmp_dir/workspace" \
    --binary "$runtime_root/bin/agl" \
    --config "$tmp_dir/config/local.toml" \
    --socket "$tmp_dir/state/daemon/agl.sock" \
    --workspace-root "$tmp_dir/workspace" \
    >"$tmp_dir/native-$label.out" \
    2>"$tmp_dir/native-$label.err" || status=$?
  [[ "$status" -eq 1 ]] || ci_fail "daemon installer accepted $label native bundle"
  grep -F "invalid exact native inference bundle" "$tmp_dir/native-$label.err" >/dev/null ||
    ci_fail "$label native bundle rejection was not actionable"
}

native_fixture="$runtime_generation/agl-inference-native/sha256-$fixture_bundle_digest"
chmod 0755 "$native_fixture/libggml-cpu-test.so"
expect_native_bundle_rejection writable
chmod 0555 "$native_fixture/libggml-cpu-test.so"

ln "$native_fixture/libggml-cpu-test.so" "$tmp_dir/native-hardlink"
expect_native_bundle_rejection hardlink
rm -f -- "$tmp_dir/native-hardlink"

chmod 0755 "$native_fixture"
ln -s /dev/null "$native_fixture/unexpected"
chmod 0555 "$native_fixture"
expect_native_bundle_rejection symlink
chmod 0755 "$native_fixture"
rm -f -- "$native_fixture/unexpected"
mv "$native_fixture/libmtmd.so.0" "$tmp_dir/libmtmd.so.0"
chmod 0555 "$native_fixture"
expect_native_bundle_rejection missing
chmod 0755 "$native_fixture"
mv "$tmp_dir/libmtmd.so.0" "$native_fixture/libmtmd.so.0"
chmod 0555 "$native_fixture"

ln "$runtime_generation/agl-inference-worker" "$tmp_dir/worker-hardlink"
hardlinked_worker_status=0
"$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-hardlinked-worker-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$runtime_root/bin/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/hardlinked-worker.out" \
  2>"$tmp_dir/hardlinked-worker.err" || hardlinked_worker_status=$?
rm -f -- "$tmp_dir/worker-hardlink"
[[ "$hardlinked_worker_status" -eq 1 ]] ||
  ci_fail "daemon installer accepted a hard-linked inference worker"
grep -F "must resolve through an immutable runtime bundle" \
  "$tmp_dir/hardlinked-worker.err" >/dev/null ||
  ci_fail "hard-linked inference worker rejection was not actionable"

ln -s ../libexec/agentlibre/current/agl-inference-worker \
  "$runtime_root/bin/agl-inference-worker"
public_worker_status=0
"$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-public-worker-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$runtime_root/bin/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/public-worker.out" \
  2>"$tmp_dir/public-worker.err" || public_worker_status=$?
rm -f -- "$runtime_root/bin/agl-inference-worker"
[[ "$public_worker_status" -eq 1 ]] ||
  ci_fail "daemon installer accepted a public inference worker symlink"
grep -F "must resolve through an immutable runtime bundle" \
  "$tmp_dir/public-worker.err" >/dev/null ||
  ci_fail "public inference worker rejection was not actionable"

writable_ancestor_status=0
chmod 0775 "$runtime_root/libexec"
"$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-writable-ancestor-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$runtime_root/bin/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/writable-ancestor.out" \
  2>"$tmp_dir/writable-ancestor.err" || writable_ancestor_status=$?
chmod 0755 "$runtime_root/libexec"
[[ "$writable_ancestor_status" -eq 1 ]] ||
  ci_fail "daemon installer accepted a group-writable managed ancestor"
grep -F "managed runtime ancestor must not be group/other writable" \
  "$tmp_dir/writable-ancestor.err" >/dev/null ||
  ci_fail "group-writable managed ancestor rejection was not actionable"

umask_runtime_root="$tmp_dir/umask-zero-runtime"
umask_runtime_generation="$umask_runtime_root/libexec/agentlibre/generations/generation-test"
(umask 000; mkdir -p "$umask_runtime_root/bin" "$umask_runtime_generation")
printf '#!/usr/bin/env bash\nexit 0\n' >"$umask_runtime_generation/agl"
printf '#!/usr/bin/env bash\nexit 0\n' >"$umask_runtime_generation/agl-process-launcher"
create_worker_fixture "$umask_runtime_generation/agl-inference-worker"
create_native_bundle_fixture "$umask_runtime_generation"
create_runtime_manifest_fixture "$umask_runtime_generation"
chmod 0555 \
  "$umask_runtime_generation/agl" \
  "$umask_runtime_generation/agl-process-launcher" \
  "$umask_runtime_generation/agl-inference-worker"
chmod 0555 "$umask_runtime_generation"
ln -s generations/generation-test "$umask_runtime_root/libexec/agentlibre/current"
ln -s ../libexec/agentlibre/current/agl "$umask_runtime_root/bin/agl"
ln -s ../libexec/agentlibre/current/agl-process-launcher \
  "$umask_runtime_root/bin/agl-process-launcher"
umask_runtime_status=0
"$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-umask-zero-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$umask_runtime_root/bin/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/umask-zero.out" \
  2>"$tmp_dir/umask-zero.err" || umask_runtime_status=$?
[[ "$umask_runtime_status" -eq 1 ]] ||
  ci_fail "daemon installer accepted a runtime created under umask 000"
grep -F "managed runtime ancestor must not be group/other writable" \
  "$tmp_dir/umask-zero.err" >/dev/null ||
  ci_fail "umask-000 runtime rejection was not actionable"

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
  --binary "$runtime_root/bin/agl" \
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

mkdir -p "$tmp_dir/mutable"
printf '#!/usr/bin/env bash\nexit 0\n' >"$tmp_dir/mutable/agl"
printf '#!/usr/bin/env bash\nexit 0\n' >"$tmp_dir/mutable/agl-process-launcher"
printf '#!/usr/bin/env bash\nexit 0\n' >"$tmp_dir/mutable/agl-inference-worker"
chmod 0755 \
  "$tmp_dir/mutable/agl" \
  "$tmp_dir/mutable/agl-process-launcher" \
  "$tmp_dir/mutable/agl-inference-worker"
mutable_status=0
"$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
  --dry-run \
  --unit agl-mutable-test.service \
  --cwd "$tmp_dir/workspace" \
  --binary "$tmp_dir/mutable/agl" \
  --config "$tmp_dir/config/local.toml" \
  --socket "$tmp_dir/state/daemon/agl.sock" \
  --workspace-root "$tmp_dir/workspace" \
  >"$tmp_dir/mutable.out" 2>"$tmp_dir/mutable.err" || mutable_status=$?
[[ "$mutable_status" -eq 1 ]] ||
  ci_fail "daemon installer accepted a mutable runtime binary"
grep -F "must resolve through an immutable runtime bundle" "$tmp_dir/mutable.err" >/dev/null ||
  ci_fail "mutable runtime rejection was not actionable"

install_root="$tmp_dir/daemon-install"
mkdir -p \
  "$install_root/bin" \
  "$install_root/config" \
  "$install_root/fake-bin" \
  "$install_root/libexec/agentlibre/generations/generation-test" \
  "$install_root/state/daemon" \
  "$install_root/workspace"
chmod 0755 \
  "$install_root" \
  "$install_root/bin" \
  "$install_root/libexec" \
  "$install_root/libexec/agentlibre" \
  "$install_root/libexec/agentlibre/generations"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'if [[ "${AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE:-}" == "1" && "${FAKE_IDENTITY_FAIL:-}" == "1" ]]; then' \
  '  exit 43' \
  'fi' \
  'exit 0' \
  >"$install_root/libexec/agentlibre/generations/generation-test/agl"
printf '#!/usr/bin/env bash\nexit 0\n' \
  >"$install_root/libexec/agentlibre/generations/generation-test/agl-process-launcher"
create_worker_fixture \
  "$install_root/libexec/agentlibre/generations/generation-test/agl-inference-worker"
create_native_bundle_fixture \
  "$install_root/libexec/agentlibre/generations/generation-test"
create_runtime_manifest_fixture \
  "$install_root/libexec/agentlibre/generations/generation-test"
chmod 0555 \
  "$install_root/libexec/agentlibre/generations/generation-test/agl" \
  "$install_root/libexec/agentlibre/generations/generation-test/agl-process-launcher" \
  "$install_root/libexec/agentlibre/generations/generation-test/agl-inference-worker"
chmod 0555 "$install_root/libexec/agentlibre/generations/generation-test"
ln -s generations/generation-test "$install_root/libexec/agentlibre/current"
ln -s ../libexec/agentlibre/current/agl "$install_root/bin/agl"
ln -s ../libexec/agentlibre/current/agl-process-launcher \
  "$install_root/bin/agl-process-launcher"
printf '[backend]\nkind = "llama_cpp"\n' >"$install_root/config/local.toml"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >>"${AGL_TEST_SYSTEMCTL_LOG:?}"\n' \
  >"$install_root/fake-bin/systemctl"
chmod 0755 "$install_root/fake-bin/systemctl"
chmod 0755 "$install_root/state" "$install_root/state/daemon"

identity_status=0
env \
  HOME="$install_root/home" \
  XDG_CONFIG_HOME="$install_root/config-home" \
  AGL_TEST_SYSTEMCTL_LOG="$install_root/systemctl.log" \
  FAKE_IDENTITY_FAIL=1 \
  PATH="$install_root/fake-bin:$PATH" \
  "$AGL_CI_REPO_ROOT/scripts/agentlibre-daemon-systemd-service.sh" \
    --unit agl-install-test.service \
    --cwd "$install_root/workspace" \
    --binary "$install_root/bin/agl" \
    --config "$install_root/config/local.toml" \
    --socket "$install_root/state/daemon/agl.sock" \
    --workspace-root "$install_root/workspace" \
    >"$install_root/identity.out" \
    2>"$install_root/identity.err" || identity_status=$?
[[ "$identity_status" -eq 1 ]] ||
  ci_fail "daemon installer accepted a mismatched runtime bundle"
grep -F "do not have matching build identities" "$install_root/identity.err" >/dev/null ||
  ci_fail "mismatched runtime bundle rejection was not actionable"
[[ "$(stat -c '%a' -- "$install_root/state/daemon")" == "755" ]] ||
  ci_fail "runtime identity rejection mutated the socket parent"
[[ ! -e "$install_root/config-home/systemd/user/agl-install-test.service" ]] ||
  ci_fail "runtime identity rejection installed a service unit"

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
  --binary "$runtime_root/bin/agl" \
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
