#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

fail() {
  printf 'agl-process-owner-death-smoke: %s\n' "$*" >&2
  exit 1
}

[[ "$(uname -s)" == "Linux" ]] || fail "the designated owner-death smoke requires Linux"
command -v cargo >/dev/null 2>&1 || fail "cargo is required"

cd "$repo_root"
printf 'kernel=%s\n' "$(uname -srvm)"

cargo build -p agl-process-launcher --bins --features native-test-fixtures
target_dir="${CARGO_TARGET_DIR:-target}"
if [[ "$target_dir" != /* ]]; then
  target_dir="$repo_root/$target_dir"
fi
launcher="$target_dir/debug/agl-process-launcher"
[[ -x "$launcher" ]] || fail "process launcher was not built at $launcher"

set +e
doctor_output="$(AGL_PROCESS_LAUNCH_DOCTOR=1 "$launcher" 2>&1)"
doctor_status=$?
set -e
printf 'launcher_doctor_status=%s\n%s\n' "$doctor_status" "$doctor_output"
if [[ "$doctor_status" -ne 0 ]]; then
  fail "required namespace/pidfd preflight is unsupported; owner-death acceptance cannot skip"
fi

cargo test \
  -p agl-process-launcher \
  --features native-test-fixtures \
  --test owner_death_native \
  native_owner_death_and_descendant_cleanup_smoke \
  -- \
  --ignored \
  --exact \
  --nocapture

printf 'agl-process-owner-death-smoke: passed\n'
