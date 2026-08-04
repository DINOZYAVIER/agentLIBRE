#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cargo
ci_need_tool ldd
ci_need_tool readelf

ci_cd_repo

if [[ "${AGL_CI_SKIP_PREPARE:-0}" != "1" ]]; then
  "$script_dir/prepare.sh"
fi

agl_bin="$AGL_CI_REPO_ROOT/target/release/agl"
launcher_bin="$AGL_CI_REPO_ROOT/target/release/agl-process-launcher"
worker_bin="$AGL_CI_REPO_ROOT/target/release/agl-inference-worker"
native_bundle_base="$AGL_CI_REPO_ROOT/target/release/agl-inference-native"
cleanup_smoke_home=0
isolated_daemon_pid=""
if [[ -n "${AGL_CI_SMOKE_HOME:-}" ]]; then
  smoke_home="$AGL_CI_SMOKE_HOME"
  mkdir -p "$smoke_home"
else
  smoke_home="$(mktemp -d "${TMPDIR:-/tmp}/agl-ci-smoke.XXXXXX")"
  cleanup_smoke_home=1
fi

cleanup_release_smoke() {
  if [[ -n "$isolated_daemon_pid" ]] && kill -0 "$isolated_daemon_pid" 2>/dev/null; then
    kill "$isolated_daemon_pid" 2>/dev/null || true
    wait "$isolated_daemon_pid" 2>/dev/null || true
  fi
  if (( cleanup_smoke_home == 1 )); then
    rm -rf -- "$smoke_home"
  fi
}
trap cleanup_release_smoke EXIT

ci_section "Building release runtime bundle"
ci_run cargo build --locked --release \
  -p agl-cli \
  -p agl-inference-worker \
  -p agl-process-launcher \
  --bin agl \
  --bin agl-inference-worker \
  --bin agl-process-launcher

[[ -x "$agl_bin" ]] || ci_fail "missing release binary: $agl_bin"
[[ -x "$launcher_bin" ]] || ci_fail "missing release binary: $launcher_bin"
[[ -x "$worker_bin" ]] || ci_fail "missing release binary: $worker_bin"
[[ -d "$native_bundle_base" && ! -L "$native_bundle_base" ]] ||
  ci_fail "missing release native inference bundle base: $native_bundle_base"

resolve_native_bundle_relative() {
  local worker="$1"
  local runpath
  local component
  local -a matches=()
  runpath="$(readelf -d "$worker" | sed -n -E 's/.*\((RPATH|RUNPATH)\).*\[(.*)\]/\2/p')"
  while IFS= read -r component; do
    case "$component" in
      '$ORIGIN'/agl-inference-native/sha256-[0-9a-f][0-9a-f]*)
        [[ "$component" =~ ^\$ORIGIN/(agl-inference-native/sha256-[0-9a-f]{64})$ ]] ||
          ci_fail "worker has an invalid content-addressed native bundle RUNPATH: $component"
        matches+=("${BASH_REMATCH[1]}")
        ;;
    esac
  done < <(printf '%s\n' "$runpath" | tr ':' '\n')
  (( ${#matches[@]} == 1 )) ||
    ci_fail "worker must select exactly one content-addressed native bundle leaf"
  printf '%s\n' "${matches[0]}"
}

native_bundle_relative="$(resolve_native_bundle_relative "$worker_bin")"
native_bundle="$AGL_CI_REPO_ROOT/target/release/$native_bundle_relative"
[[ -d "$native_bundle" && ! -L "$native_bundle" ]] ||
  ci_fail "missing selected release native inference bundle: $native_bundle"

validate_native_bundle() {
  local entry
  local name
  local count=0
  local cpu_count=0
  local required
  [[ "$(stat -c '%a' -- "$native_bundle")" == 555 ]] ||
    ci_fail "native inference bundle directory is not sealed to 0555"
  shopt -s nullglob
  for entry in "$native_bundle"/* "$native_bundle"/.[!.]* "$native_bundle"/..?*; do
    name="${entry##*/}"
    [[ -f "$entry" && ! -L "$entry" && "$(stat -c '%h' -- "$entry")" == 1 ]] ||
      ci_fail "native inference bundle entry is not an exact single-link file: $entry"
    [[ "$(stat -c '%a' -- "$entry")" == 555 ]] ||
      ci_fail "native inference bundle entry is not sealed to 0555: $entry"
    case "$name" in
      libllama-common.so.0 | libmtmd.so.0 | libllama.so.0 | libggml.so.0 | libggml-base.so.0 | libggml-vulkan.so) ;;
      libggml-cpu-*.so) cpu_count=$((cpu_count + 1)) ;;
      *) ci_fail "native inference bundle contains an unexpected file: $entry" ;;
    esac
    count=$((count + 1))
    (( count <= 64 )) || ci_fail "native inference bundle exceeds its file bound"
  done
  shopt -u nullglob
  (( cpu_count > 0 )) || ci_fail "native inference bundle has no CPU plugin"
  for required in libllama-common.so.0 libmtmd.so.0 libllama.so.0 libggml.so.0 libggml-base.so.0; do
    [[ -f "$native_bundle/$required" && ! -L "$native_bundle/$required" ]] ||
      ci_fail "native inference bundle is missing $required"
  done
}

check_elf_search_paths() {
  local object="$1"
  local runpath
  local component
  runpath="$(readelf -d "$object" | sed -n -E 's/.*\((RPATH|RUNPATH)\).*\[(.*)\]/\2/p')"
  [[ "$runpath" != *"$AGL_CI_REPO_ROOT"* && "$runpath" != *"/target/"* ]] ||
    ci_fail "ELF search path points into a mutable build tree: $object: $runpath"
  while IFS= read -r component; do
    [[ -n "$component" ]] || ci_fail "ELF search path contains a current-directory entry: $object"
    case "$component" in
      '$ORIGIN' | '$ORIGIN/'*) ;;
      /nix/store/*)
        [[ -e "$component" ]] || ci_fail "ELF references a missing Nix store runtime path: $object: $component"
        printf 'Nix GC-sensitive runtime reference: %s -> %s\n' "$object" "$component"
        ;;
      /*)
        [[ -e "$component" ]] || ci_fail "ELF references a missing absolute runtime path: $object: $component"
        ;;
      *) ci_fail "ELF search path is relative or uses an unsupported token: $object: $component" ;;
    esac
  done < <(printf '%s\n' "$runpath" | tr ':' '\n')
}

validate_native_bundle
check_elf_search_paths "$worker_bin"
for bundled_elf in "$native_bundle"/*; do
  check_elf_search_paths "$bundled_elf"
done

ci_section "Checking isolated release binary link metadata"
host_dependency_tree="$(cargo tree --locked -p agl-cli --edges normal)"
[[ "$host_dependency_tree" != *"agl-llama-cpp-sys"* ]] ||
  ci_fail "agl-cli retains a normal dependency on native inference bindings"

host_native_metadata="$(readelf -d "$agl_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ -z "$host_native_metadata" ]] ||
  ci_fail "$agl_bin retained native inference linkage or a native runtime search path: $host_native_metadata"

worker_native_metadata="$(readelf -d "$worker_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ "$worker_native_metadata" == *"libllama"* ]] ||
  ci_fail "$worker_bin is not linked to libllama"
[[ "$worker_native_metadata" == *"libggml"* ]] ||
  ci_fail "$worker_bin is not linked to libggml"
[[ "$worker_native_metadata" == *"RUNPATH"* ]] ||
  ci_fail "$worker_bin has no native inference runtime search path"
[[ "$worker_native_metadata" == *"\$ORIGIN/$native_bundle_relative"* ]] ||
  ci_fail "$worker_bin does not resolve native inference only through its exact sibling bundle"
printf '%s\n' "$worker_native_metadata"

startup_libraries="$(ldd "$agl_bin")"
[[ "$startup_libraries" != *"libllama"* && "$startup_libraries" != *"libggml"* ]] ||
  ci_fail "$agl_bin loads native inference libraries in the host process"
[[ "$startup_libraries" != *"libvulkan"* ]] ||
  ci_fail "$agl_bin has a hard Vulkan startup dependency; accelerator backends must load dynamically"
printf '%s\n' "$startup_libraries"

worker_startup_libraries="$(ldd "$worker_bin")"
[[ "$worker_startup_libraries" != *"not found"* ]] ||
  ci_fail "$worker_bin has unresolved native inference libraries: $worker_startup_libraries"
[[ "$worker_startup_libraries" == *"libllama"* && "$worker_startup_libraries" == *"libggml"* ]] ||
  ci_fail "$worker_bin does not resolve its native inference libraries"
[[ "$worker_startup_libraries" != *"libvulkan"* ]] ||
  ci_fail "$worker_bin has a hard Vulkan startup dependency; accelerator backends must load dynamically"
printf '%s\n' "$worker_startup_libraries"
for required in libllama-common.so.0 libmtmd.so.0 libllama.so.0 libggml.so.0 libggml-base.so.0; do
  resolved="$(printf '%s\n' "$worker_startup_libraries" | awk -v library="$required" '$1 == library { print $3 }')"
  [[ -n "$resolved" && "$(readlink -f -- "$resolved")" == "$native_bundle/$required" ]] ||
    ci_fail "$worker_bin resolved $required outside its exact sibling bundle: ${resolved:-missing}"
done

ci_section "Checking exact release bundle identities"
ci_run env AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE=1 "$agl_bin"

ci_section "Checking isolated daemon runtime mappings"
daemon_socket="$smoke_home/runtime-map.sock"
daemon_stdout="$smoke_home/runtime-map.stdout"
daemon_stderr="$smoke_home/runtime-map.stderr"
"$agl_bin" --home "$smoke_home" serve --socket "$daemon_socket" \
  >"$daemon_stdout" 2>"$daemon_stderr" &
isolated_daemon_pid=$!
for _ in {1..100}; do
  [[ -S "$daemon_socket" ]] && break
  if ! kill -0 "$isolated_daemon_pid" 2>/dev/null; then
    wait "$isolated_daemon_pid" 2>/dev/null || true
    ci_fail "isolated release daemon exited before binding: $(cat "$daemon_stderr")"
  fi
  sleep 0.05
done
[[ -S "$daemon_socket" ]] ||
  ci_fail "isolated release daemon did not bind its private test socket"
[[ -r "/proc/$isolated_daemon_pid/maps" ]] ||
  ci_fail "cannot inspect isolated release daemon mappings"
host_runtime_native_mappings="$(grep -E '/(libllama|libggml[^/]*|libvulkan[^/]*)\.so([.]|$)' "/proc/$isolated_daemon_pid/maps" || true)"
[[ -z "$host_runtime_native_mappings" ]] ||
  ci_fail "release daemon mapped native inference libraries in the host process: $host_runtime_native_mappings"
kill "$isolated_daemon_pid"
wait "$isolated_daemon_pid" 2>/dev/null || true
isolated_daemon_pid=""

ci_section "Checking public CLI surface"
ci_run "$agl_bin" --version
ci_run "$agl_bin" --help
ci_run "$agl_bin" config paths --home "$smoke_home"
ci_run "$agl_bin" model --help
ci_run "$agl_bin" --home "$smoke_home" model list --json

expect_failure_contains() {
  local expected="$1"
  shift
  local output
  set +e
  output="$("$@" 2>&1)"
  local status=$?
  set -e
  [[ "$status" -ne 0 ]] || ci_fail "command unexpectedly succeeded: $*"
  [[ "$output" == *"$expected"* ]] || ci_fail "command output did not contain '$expected': $output"
}

expect_failure_contains 'unknown command `setup`' "$agl_bin" setup
expect_failure_contains 'unknown command `doctor`' "$agl_bin" doctor

ci_section "Release CLI smoke passed"
