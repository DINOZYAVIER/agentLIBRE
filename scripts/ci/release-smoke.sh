#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cargo
ci_need_tool readelf

ci_cd_repo

if [[ "${AGL_CI_SKIP_PREPARE:-0}" != "1" ]]; then
  "$script_dir/prepare.sh"
fi

agl_bin="$AGL_CI_REPO_ROOT/target/release/agl"
if [[ -n "${AGL_CI_SMOKE_HOME:-}" ]]; then
  smoke_home="$AGL_CI_SMOKE_HOME"
  mkdir -p "$smoke_home"
else
  smoke_home="$(mktemp -d "${TMPDIR:-/tmp}/agl-ci-smoke.XXXXXX")"
  trap 'rm -rf "$smoke_home"' EXIT
fi

ci_section "Building release CLI"
ci_run cargo build --locked --release -p agl-cli

[[ -x "$agl_bin" ]] || ci_fail "missing release binary: $agl_bin"

ci_section "Checking release binary link metadata"
linked_libraries="$(readelf -d "$agl_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ "$linked_libraries" == *"libllama"* ]] || ci_fail "$agl_bin is not linked to libllama"
[[ "$linked_libraries" == *"libggml"* ]] || ci_fail "$agl_bin is not linked to libggml"
printf '%s\n' "$linked_libraries"

ci_section "Checking public CLI surface"
ci_run "$agl_bin" --version
ci_run "$agl_bin" --help
ci_run "$agl_bin" config paths --home "$smoke_home"

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
expect_failure_contains 'unknown command `model`' "$agl_bin" model pull

ci_section "Release CLI smoke passed"
