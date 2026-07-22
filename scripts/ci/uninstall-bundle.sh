#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_cd_repo
ci_need_tool cc

tmp_dir="$(mktemp -d)"
cleanup() {
  if [[ -n "${live_pid:-}" ]]; then
    kill "$live_pid" 2>/dev/null || true
    wait "$live_pid" 2>/dev/null || true
  fi
  chmod -R u+w "$tmp_dir" 2>/dev/null || true
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

fake_bin="$tmp_dir/fake-bin"
fake_systemd_state="$tmp_dir/fake-systemd-state"
fake_systemd_units="$tmp_dir/config/systemd/user"
fake_systemctl_log="$tmp_dir/systemctl.log"
mkdir -p "$fake_bin" "$fake_systemd_state" "$fake_systemd_units"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'printf '\''%q '\'' "$@" >>"${FAKE_SYSTEMCTL_LOG:?}"' \
  'printf '\''\n'\'' >>"$FAKE_SYSTEMCTL_LOG"' \
  '[[ "${1:-}" == --user ]] || exit 64' \
  'shift' \
  'command_name="${1:-}"' \
  'shift || true' \
  'case "$command_name" in' \
  '  show)' \
  '    [[ "${FAKE_SYSTEMCTL_QUERY_FAIL:-0}" != 1 ]] || exit 1' \
  '    unit="${1:?missing unit}"' \
  '    unit_file="${FAKE_SYSTEMD_UNIT_DIR:?}/$unit"' \
  '    if [[ -f "$unit_file" ]]; then load=loaded; fragment="$unit_file"; else load=not-found; fragment=""; fi' \
  '    active="$(cat "${FAKE_SYSTEMD_STATE:?}/$unit" 2>/dev/null || printf inactive)"' \
  '    [[ "$load" == loaded ]] || active=inactive' \
  '    exec_start=""' \
  '    if [[ "$unit" == agentlibre-daemon.service && -f "$unit_file" ]]; then' \
  '      command_line="$(sed -n '\''s/^ExecStart="\([^\"]*\)" \(.*\)$/\1 \2/p'\'' "$unit_file")"' \
  '      executable="${command_line%% *}"' \
  '      exec_start="{ path=$executable ; argv[]=$command_line ; ignore_errors=no ; }"' \
  '    fi' \
  '    printf '\''LoadState=%s\nActiveState=%s\nFragmentPath=%s\nDropInPaths=%s\nExecStart=%s\n'\'' "$load" "$active" "$fragment" "${FAKE_SYSTEMCTL_DROPINS:-}" "$exec_start"' \
  '    ;;' \
  '  stop)' \
  '    [[ "${FAKE_SYSTEMCTL_STOP_FAIL:-0}" != 1 ]] || exit 1' \
  '    for unit in "$@"; do' \
  '      if [[ "${FAKE_SYSTEMCTL_STICKY_UNIT:-}" != "$unit" ]]; then printf inactive >"${FAKE_SYSTEMD_STATE:?}/$unit"; fi' \
  '    done' \
  '    ;;' \
  '  *) exit 64 ;;' \
  'esac' \
  >"$fake_bin/systemctl"
chmod 0755 "$fake_bin/systemctl"

write_executable() {
  local path="$1"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$path"
  chmod 0555 "$path"
}

seed_obsolete_generation() {
  local root="$1"
  local name="$2"
  local generation="$root/libexec/agentlibre/generations/$name"
  mkdir -p "$generation"
  write_executable "$generation/agl"
  write_executable "$generation/agl-process-launcher"
  chmod 0555 "$generation"
}

seed_current_generation() {
  local root="$1"
  local name="$2"
  local digest
  local generation="$root/libexec/agentlibre/generations/$name"
  digest="$(printf 'a%.0s' {1..64})"
  mkdir -p "$generation/agl-inference-native/sha256-$digest"
  write_executable "$generation/agl"
  write_executable "$generation/agl-process-launcher"
  write_executable "$generation/agl-inference-worker"
  for library in \
    libllama-common.so.0 \
    libmtmd.so.0 \
    libllama.so.0 \
    libggml.so.0 \
    libggml-base.so.0 \
    libggml-cpu-test.so
  do
    : >"$generation/agl-inference-native/sha256-$digest/$library"
    chmod 0555 "$generation/agl-inference-native/sha256-$digest/$library"
  done
  chmod 0555 \
    "$generation/agl-inference-native/sha256-$digest" \
    "$generation/agl-inference-native" \
    "$generation"
}

seed_surface() {
  local root="$1"
  local current="$2"
  mkdir -p "$root/bin"
  : >"$root/.agentlibre-runtime.lock"
  chmod 0600 "$root/.agentlibre-runtime.lock"
  ln -s -- "generations/$current" "$root/libexec/agentlibre/current"
  ln -s -- "../libexec/agentlibre/current/agl" "$root/bin/agl"
  ln -s -- "../libexec/agentlibre/current/agl-process-launcher" \
    "$root/bin/agl-process-launcher"
}

run_uninstaller() {
  local root="$1"
  shift
  env \
    PATH="$fake_bin:$PATH" \
    HOME="$tmp_dir/home" \
    XDG_CONFIG_HOME="$tmp_dir/config" \
    FAKE_SYSTEMD_STATE="$fake_systemd_state" \
    FAKE_SYSTEMD_UNIT_DIR="$fake_systemd_units" \
    FAKE_SYSTEMCTL_LOG="$fake_systemctl_log" \
    scripts/uninstall-agl-cargo.sh --root "$root" "$@"
}

seed_standard_units() {
  local root="$1"
  cat >"$fake_systemd_units/agentlibre-daemon.service" <<EOF
[Unit]
Requires=agentlibre-daemon.socket
[Service]
ExecStart="$root/bin/agl" serve --systemd-activation --config /tmp/local.toml
EOF
  cat >"$fake_systemd_units/agentlibre-daemon.socket" <<'EOF'
[Socket]
Accept=no
Service=agentlibre-daemon.service
EOF
  chmod 0644 \
    "$fake_systemd_units/agentlibre-daemon.service" \
    "$fake_systemd_units/agentlibre-daemon.socket"
}

assert_surface_present() {
  local root="$1"
  [[ -L "$root/bin/agl" && -L "$root/bin/agl-process-launcher" &&
    -L "$root/libexec/agentlibre/current" &&
    -f "$root/.agentlibre-runtime.lock" ]] ||
    ci_fail "managed runtime surface was unexpectedly changed: $root"
}

obsolete_root="$tmp_dir/obsolete"
seed_obsolete_generation "$obsolete_root" generation-olda
seed_obsolete_generation "$obsolete_root" generation-oldb
seed_surface "$obsolete_root" generation-oldb
preview="$(run_uninstaller "$obsolete_root")"
[[ "$preview" == *"generations: 2 (current=0 obsolete=2)"* ]] ||
  ci_fail "obsolete preview omitted its exact generation counts: $preview"
[[ "$preview" == *"preview only; rerun with --apply"* ]] ||
  ci_fail "uninstaller did not default to preview: $preview"
assert_surface_present "$obsolete_root"
run_uninstaller "$obsolete_root" --apply >"$tmp_dir/obsolete-apply.out"
[[ ! -e "$obsolete_root/libexec/agentlibre" &&
  ! -e "$obsolete_root/bin/agl" &&
  ! -e "$obsolete_root/bin/agl-process-launcher" &&
  ! -e "$obsolete_root/.agentlibre-runtime.lock" ]] ||
  ci_fail "obsolete managed runtime was not removed"
[[ -d "$obsolete_root/bin" && -d "$obsolete_root/libexec" ]] ||
  ci_fail "uninstaller removed shared install ancestors"

current_root="$tmp_dir/current"
seed_current_generation "$current_root" generation-current
seed_obsolete_generation "$current_root" generation-old
seed_surface "$current_root" generation-current
mkdir -p "$current_root/config-preserved"
run_uninstaller "$current_root" --apply >"$tmp_dir/current-apply.out"
[[ ! -e "$current_root/libexec/agentlibre" && -d "$current_root/config-preserved" ]] ||
  ci_fail "current uninstall removed the wrong scope"

active_unit_root="$tmp_dir/active-unit"
seed_obsolete_generation "$active_unit_root" generation-old
seed_surface "$active_unit_root" generation-old
seed_standard_units "$active_unit_root"
printf active >"$fake_systemd_state/agentlibre-daemon.socket"
printf active >"$fake_systemd_state/agentlibre-daemon.service"
: >"$fake_systemctl_log"
active_preview="$(run_uninstaller "$active_unit_root")"
[[ "$active_preview" == *"stop before removal: systemd user unit agentlibre-daemon.socket"* &&
  "$active_preview" == *"stop before removal: systemd user unit agentlibre-daemon.service"* ]] ||
  ci_fail "active-unit preview omitted automatic stop plan: $active_preview"
if grep -F -- "stop agentlibre-daemon" "$fake_systemctl_log" >/dev/null; then
  ci_fail "uninstall preview changed systemd state"
fi
run_uninstaller "$active_unit_root" --apply >"$tmp_dir/active-unit.out"
[[ ! -e "$active_unit_root/libexec/agentlibre" ]] ||
  ci_fail "automatic standard-unit stop did not allow uninstall"
[[ "$(cat "$fake_systemd_state/agentlibre-daemon.socket")" == inactive &&
  "$(cat "$fake_systemd_state/agentlibre-daemon.service")" == inactive ]] ||
  ci_fail "automatic uninstall did not stop the standard unit pair"
[[ -f "$fake_systemd_units/agentlibre-daemon.socket" &&
  -f "$fake_systemd_units/agentlibre-daemon.service" ]] ||
  ci_fail "automatic uninstall removed preserved standard unit files"
grep -F -- "stop agentlibre-daemon.socket agentlibre-daemon.service" \
  "$fake_systemctl_log" >/dev/null ||
  ci_fail "automatic uninstall did not stop only the exact standard unit pair"

stop_failure_root="$tmp_dir/stop-failure"
seed_obsolete_generation "$stop_failure_root" generation-old
seed_surface "$stop_failure_root" generation-old
seed_standard_units "$stop_failure_root"
printf active >"$fake_systemd_state/agentlibre-daemon.socket"
printf active >"$fake_systemd_state/agentlibre-daemon.service"
stop_failure_status=0
FAKE_SYSTEMCTL_STOP_FAIL=1 run_uninstaller "$stop_failure_root" --apply \
  >"$tmp_dir/stop-failure.out" 2>"$tmp_dir/stop-failure.err" || stop_failure_status=$?
[[ "$stop_failure_status" -eq 1 ]] || ci_fail "failed standard-unit stop did not abort uninstall"
assert_surface_present "$stop_failure_root"
printf inactive >"$fake_systemd_state/agentlibre-daemon.socket"
printf inactive >"$fake_systemd_state/agentlibre-daemon.service"

query_failure_root="$tmp_dir/query-failure"
seed_obsolete_generation "$query_failure_root" generation-old
seed_surface "$query_failure_root" generation-old
seed_standard_units "$query_failure_root"
printf active >"$fake_systemd_state/agentlibre-daemon.socket"
printf active >"$fake_systemd_state/agentlibre-daemon.service"
query_failure_status=0
FAKE_SYSTEMCTL_QUERY_FAIL=1 run_uninstaller "$query_failure_root" --apply \
  >"$tmp_dir/query-failure.out" 2>"$tmp_dir/query-failure.err" || query_failure_status=$?
[[ "$query_failure_status" -eq 1 ]] || ci_fail "systemd query failure did not abort uninstall"
assert_surface_present "$query_failure_root"
printf inactive >"$fake_systemd_state/agentlibre-daemon.socket"
printf inactive >"$fake_systemd_state/agentlibre-daemon.service"

locked_root="$tmp_dir/locked"
seed_obsolete_generation "$locked_root" generation-old
seed_surface "$locked_root" generation-old
exec {held_lock_fd}<>"$locked_root/.agentlibre-runtime.lock"
flock --exclusive "$held_lock_fd"
locked_status=0
run_uninstaller "$locked_root" --apply \
  >"$tmp_dir/locked.out" 2>"$tmp_dir/locked.err" || locked_status=$?
flock --unlock "$held_lock_fd"
exec {held_lock_fd}>&-
[[ "$locked_status" -eq 1 ]] || ci_fail "held installer lock did not block uninstall"
grep -F "another runtime bundle operation holds" "$tmp_dir/locked.err" >/dev/null ||
  ci_fail "held-lock refusal omitted its reason"
assert_surface_present "$locked_root"

live_root="$tmp_dir/live-process"
seed_obsolete_generation "$live_root" generation-live
printf '%s\n' \
  '#include <unistd.h>' \
  'int main(void) { sleep(30); return 0; }' \
  >"$tmp_dir/sleeper.c"
cc "$tmp_dir/sleeper.c" -o "$tmp_dir/sleeper"
chmod u+w \
  "$live_root/libexec/agentlibre/generations/generation-live" \
  "$live_root/libexec/agentlibre/generations/generation-live/agl"
cp -- "$tmp_dir/sleeper" "$live_root/libexec/agentlibre/generations/generation-live/agl"
chmod 0555 \
  "$live_root/libexec/agentlibre/generations/generation-live/agl" \
  "$live_root/libexec/agentlibre/generations/generation-live"
seed_surface "$live_root" generation-live
"$live_root/libexec/agentlibre/generations/generation-live/agl" 30 &
live_pid=$!
for _ in {1..100}; do
  [[ "$(readlink -- "/proc/$live_pid/exe" 2>/dev/null || true)" == \
    "$live_root/libexec/agentlibre/generations/generation-live/agl" ]] && break
  sleep 0.01
done
live_status=0
run_uninstaller "$live_root" --apply \
  >"$tmp_dir/live.out" 2>"$tmp_dir/live.err" || live_status=$?
[[ "$live_status" -eq 1 ]] || ci_fail "running managed executable did not block uninstall"
if ! grep -F "running process uses the runtime bundle: pid=$live_pid" \
  "$tmp_dir/live.out" "$tmp_dir/live.err" >/dev/null; then
  ci_fail "running-process refusal omitted its evidence: $(cat "$tmp_dir/live.out" "$tmp_dir/live.err")"
fi
assert_surface_present "$live_root"
kill "$live_pid"
wait "$live_pid" 2>/dev/null || true
live_pid=""

escape_root="$tmp_dir/escape"
escape_target="$tmp_dir/escape-target"
seed_obsolete_generation "$escape_root" generation-old
seed_obsolete_generation "$escape_target" generation-outside
seed_surface "$escape_root" generation-old
rm -f "$escape_root/libexec/agentlibre/current"
ln -s -- "$escape_target/libexec/agentlibre/generations/generation-outside" \
  "$escape_root/libexec/agentlibre/current"
escape_status=0
run_uninstaller "$escape_root" --apply \
  >"$tmp_dir/escape.out" 2>"$tmp_dir/escape.err" || escape_status=$?
[[ "$escape_status" -eq 1 ]] || ci_fail "escaping current pointer did not block uninstall"
[[ -d "$escape_target/libexec/agentlibre/generations/generation-outside" ]] ||
  ci_fail "escaping current pointer mutated its external target"

hardlink_root="$tmp_dir/hardlink"
seed_obsolete_generation "$hardlink_root" generation-old
seed_surface "$hardlink_root" generation-old
ln "$hardlink_root/libexec/agentlibre/generations/generation-old/agl" \
  "$tmp_dir/hardlink-alias"
hardlink_status=0
run_uninstaller "$hardlink_root" --apply \
  >"$tmp_dir/hardlink.out" 2>"$tmp_dir/hardlink.err" || hardlink_status=$?
[[ "$hardlink_status" -eq 1 ]] || ci_fail "hard-linked executable did not block uninstall"
assert_surface_present "$hardlink_root"

writable_root="$tmp_dir/writable-ancestor"
seed_obsolete_generation "$writable_root" generation-old
seed_surface "$writable_root" generation-old
chmod 0775 "$writable_root/libexec"
writable_status=0
run_uninstaller "$writable_root" --apply \
  >"$tmp_dir/writable.out" 2>"$tmp_dir/writable.err" || writable_status=$?
[[ "$writable_status" -eq 1 ]] || ci_fail "writable ancestor did not block uninstall"
assert_surface_present "$writable_root"

symlink_root="$tmp_dir/symlink-root"
symlink_outside="$tmp_dir/symlink-outside"
seed_obsolete_generation "$symlink_outside" generation-old
seed_surface "$symlink_outside" generation-old
ln -s -- "$symlink_outside" "$symlink_root"
symlink_status=0
run_uninstaller "$symlink_root" --apply \
  >"$tmp_dir/symlink.out" 2>"$tmp_dir/symlink.err" || symlink_status=$?
[[ "$symlink_status" -eq 1 ]] || ci_fail "symlinked install root did not block uninstall"
assert_surface_present "$symlink_outside"

unmanaged_root="$tmp_dir/unmanaged"
mkdir -p "$unmanaged_root/bin" "$unmanaged_root/libexec/agentlibre/generations"
write_executable "$unmanaged_root/bin/agl"
unmanaged_status=0
run_uninstaller "$unmanaged_root" --apply \
  >"$tmp_dir/unmanaged.out" 2>"$tmp_dir/unmanaged.err" || unmanaged_status=$?
[[ "$unmanaged_status" -eq 1 && -f "$unmanaged_root/bin/agl" ]] ||
  ci_fail "uninstaller accepted or mutated an unmanaged surface"

empty_root="$tmp_dir/empty"
empty_output="$(run_uninstaller "$empty_root" --apply)"
[[ "$empty_output" == *"no managed agentLIBRE runtime bundle is installed"* ]] ||
  ci_fail "empty uninstall was not idempotent: $empty_output"

echo "uninstall managed runtime bundle: passed"
