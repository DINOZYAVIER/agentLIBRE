#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

fail() {
  printf 'agl-terminal-native-linux: %s\n' "$*" >&2
  exit 1
}

[[ "$(uname -s)" == "Linux" ]] || fail "native acceptance requires Linux"
command -v cargo >/dev/null 2>&1 || fail "cargo is required"

cd "$repo_root"
printf 'kernel=%s\n' "$(uname -srvm)"
cargo build -p agl-process-launcher --bins --features native-test-fixtures --locked

target_dir="${CARGO_TARGET_DIR:-target}"
[[ "$target_dir" == /* ]] || target_dir="$repo_root/$target_dir"
launcher="$target_dir/debug/agl-process-launcher"
[[ -x "$launcher" ]] || fail "launcher was not built at $launcher"

set +e
doctor_output="$(AGL_PROCESS_LAUNCH_DOCTOR=1 "$launcher" 2>&1)"
doctor_status=$?
set -e
printf 'launcher_doctor_status=%s\n%s\n' "$doctor_status" "$doctor_output"
[[ "$doctor_status" -eq 0 ]] ||
  fail "namespace/Landlock/seccomp/pidfd/PTY preflight is unsupported"

cargo test \
  -p agl-process-launcher \
  --features native-test-fixtures \
  --test linux_native \
  --locked \
  native_linux_sandbox_process_and_pty_smoke \
  -- \
  --ignored \
  --exact \
  --nocapture

printf 'agl-terminal-native-linux: passed\n'
