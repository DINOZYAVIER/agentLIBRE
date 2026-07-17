#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

fail() {
  printf 'agl-process-linux-smoke: %s\n' "$*" >&2
  exit 1
}

[[ "$(uname -s)" == "Linux" ]] || fail "the designated native smoke requires Linux"
command -v cargo >/dev/null 2>&1 || fail "cargo is required"

cd "$repo_root"
printf 'kernel=%s\n' "$(uname -srvm)"
for diagnostic in \
  /proc/sys/kernel/unprivileged_userns_clone \
  /proc/sys/user/max_user_namespaces; do
  if [[ -r "$diagnostic" ]]; then
    printf '%s=%s\n' "$diagnostic" "$(<"$diagnostic")"
  else
    printf '%s=unavailable\n' "$diagnostic"
  fi
done

cargo build -p agl-process --bins --features native-test-fixtures
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
  fail "required namespace/Landlock/seccomp/pidfd/PTY preflight is unsupported; native acceptance cannot skip"
fi

cargo test \
  -p agl-process \
  --features native-test-fixtures \
  --test linux_native \
  native_linux_sandbox_process_and_pty_smoke \
  -- \
  --ignored \
  --exact \
  --nocapture

cargo test \
  -p agl-cli \
  --test process_pty_native \
  cli_attach_detach_reattach_and_kill_real_daemon_owned_pty \
  -- \
  --ignored \
  --exact \
  --nocapture

printf 'agl-process-linux-smoke: passed\n'
