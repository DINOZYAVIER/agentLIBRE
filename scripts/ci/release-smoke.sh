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
llama_bin="${AGL_LLAMA_CPP_BUILD_DIR:-$AGL_CI_REPO_ROOT/target/llama-cpp/build}/bin"
engine="$llama_bin/llama-server"

ci_section "Building release host and selected engine"
ci_run cargo build --locked --release -p agl-cli --bin agl
ci_run "$AGL_CI_REPO_ROOT/scripts/build-llama-cpp.sh"
[[ -x "$agl_bin" ]] || ci_fail "missing release host: $agl_bin"
[[ -x "$engine" ]] || ci_fail "missing private engine: $engine"

ci_section "Checking authority split"
host_dependency_tree="$(cargo tree --locked -p agl-cli --edges normal)"
[[ "$host_dependency_tree" != *"agl-llama-cpp-sys"* ]] ||
  ci_fail "agl-cli retains a native inference dependency"
host_native_metadata="$(readelf -d "$agl_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ -z "$host_native_metadata" ]] ||
  ci_fail "host retained native inference linkage: $host_native_metadata"
host_libraries="$(ldd "$agl_bin")"
[[ "$host_libraries" != *"libllama"* && "$host_libraries" != *"libggml"* ]] ||
  ci_fail "host loads llama.cpp libraries"
[[ "$host_libraries" != *"libvulkan"* ]] ||
  ci_fail "host has a hard Vulkan dependency"

engine_libraries="$(ldd "$engine")"
[[ "$engine_libraries" != *"not found"* ]] ||
  ci_fail "private engine has unresolved libraries: $engine_libraries"
[[ "$engine_libraries" == *"libllama"* && "$engine_libraries" == *"libggml"* ]] ||
  ci_fail "private engine does not own the llama.cpp closure"
runpath="$(readelf -d "$engine" | sed -n -E 's/.*\((RPATH|RUNPATH)\).*\[(.*)\]/\2/p')"
[[ "$runpath" == *'$ORIGIN'* && "$runpath" != *"/target/"* ]] ||
  ci_fail "private engine lacks a relocatable sibling-library path: $runpath"

inventory="$(exec 3>&1; env -i AGL_LLAMA_SERVER_INVENTORY_FD=3 "$engine")"
[[ "$inventory" == *'"schema":"agentlibre.llama-inventory/v1"'* ]] ||
  ci_fail "private engine did not return the typed inventory: $inventory"
[[ "$inventory" == *'"llama_cpp_commit"'* && "$inventory" == *'"devices"'* ]] ||
  ci_fail "private engine inventory omitted build/device identity: $inventory"

ci_section "Checking public CLI surface"
smoke_home="$(mktemp -d "${TMPDIR:-/tmp}/agl-ci-smoke.XXXXXX")"
trap 'rm -rf -- "$smoke_home"' EXIT
ci_run "$agl_bin" --version
ci_run "$agl_bin" --help
ci_run "$agl_bin" config paths --home "$smoke_home"
ci_run "$agl_bin" model --help
ci_run "$agl_bin" --home "$smoke_home" model list --json

echo "release smoke passed"
