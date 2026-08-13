#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../.." && pwd)"
cd "$repo_root"

temporary_root="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$temporary_root" 2>/dev/null || true
  rm -rf -- "$temporary_root"
}
trap cleanup EXIT

failures=0
check() {
  local label="$1"
  shift
  if ! "$@"; then
    printf 'agl178-packaging: FAIL: %s\n' "$label" >&2
    failures=$((failures + 1))
  fi
}
contains() {
  [[ "$1" == *"$2"* ]]
}
equals() {
  [[ "$1" == "$2" ]]
}

fake_bin="$temporary_root/fake-bin"
target="$temporary_root/target"
prefix="$temporary_root/terminal install %25"
mkdir -p "$fake_bin" "$target/release"
printf '#!/usr/bin/env bash\nexit 0\n' >"$fake_bin/cargo"
chmod 0755 "$fake_bin/cargo"

fixture_repo="$temporary_root/clean-terminal-source"
mkdir -p \
  "$fixture_repo/scripts/terminal" \
  "$fixture_repo/crates/agl-terminal-protocol"
cp -- scripts/terminal/install.sh "$fixture_repo/scripts/terminal/install.sh"
cp -- crates/agl-terminal-protocol/Cargo.toml \
  "$fixture_repo/crates/agl-terminal-protocol/Cargo.toml"
git -C "$fixture_repo" init -q
git -C "$fixture_repo" config user.name "AGL test"
git -C "$fixture_repo" config user.email "agl-test@example.invalid"
git -C "$fixture_repo" add .
git -C "$fixture_repo" commit -qm "terminal fixture"

write_binary() {
  local name="$1"
  local value="$2"
  printf '#!/usr/bin/env bash\nprintf "%%s\\n" "%s"\n' "$value" >"$target/release/$name"
  chmod 0755 "$target/release/$name"
}
write_binary agl-terminald service
write_binary agl-process-launcher launcher
write_binary agl-terminal ui-v1

install_once() {
  PATH="$fake_bin:$PATH" CARGO_TARGET_DIR="$target" \
    "$fixture_repo/scripts/terminal/install.sh" --prefix "$prefix"
}

first_output="$(install_once)"
first_generation="$(sed -n 's/^generation=//p' <<<"$first_output")"
manifest="$first_generation/runtime-manifest.json"
manifest_text="$(cat -- "$manifest")"
manifest_hex="$(sha256sum -- "$manifest" | awk '{print $1}')"

check "manifest uses the selected v2 schema" \
  contains "$manifest_text" '"schema": "agl-terminal.runtime-generation.v2"'
for role in agl-terminald agl-process-launcher agl-terminal; do
  check "manifest has canonical file entry for $role" \
    contains "$manifest_text" "\"path\": \"$role\""
done
check "generation name is the full sealed manifest digest" \
  equals "$(basename -- "$first_generation")" "generation-$manifest_hex"
check "one atomic current pointer selects the terminal generation" \
  equals "$(readlink -- "$prefix/libexec/agl-terminal/current" 2>/dev/null || true)" \
  "generations/$(basename -- "$first_generation")"
check "UI public link resolves through current" \
  equals "$(readlink -- "$prefix/bin/agl-terminal" 2>/dev/null || true)" \
  "../libexec/agl-terminal/current/agl-terminal"
check "service public link resolves through current" \
  equals "$(readlink -- "$prefix/bin/agl-terminald" 2>/dev/null || true)" \
  "../libexec/agl-terminal/current/agl-terminald"

unit_output="$(
  HOME="$temporary_root/home" \
  XDG_CONFIG_HOME="$temporary_root/config" \
  XDG_DATA_HOME="$temporary_root/data" \
  XDG_STATE_HOME="$temporary_root/state" \
  XDG_RUNTIME_DIR="$temporary_root/runtime" \
    scripts/terminal/systemd-user-service.sh --prefix "$prefix"
)"
unit_generation="${first_generation//\%/%%}"
check "terminal unit executes the immutable generation binary" \
  contains "$unit_output" "ExecStart=\"$unit_generation/agl-terminald\""
check "unit no longer supplies build identity as environment authority" \
  test "$(grep -c 'AGL_TERMINALD_BUILD_ID=' <<<"$unit_output" || true)" -eq 0
check "unit no longer selects a launcher through the environment" \
  test "$(grep -c 'AGL_TERMINALD_LAUNCHER=' <<<"$unit_output" || true)" -eq 0

hostile_config="$temporary_root/hostile-config"
hostile_unit_dir="$hostile_config/systemd/user"
mkdir -p "$hostile_unit_dir"
printf '# foreign unit\n' >"$hostile_unit_dir/agl-terminald.service"
hostile_status=0
HOME="$temporary_root/home" \
XDG_CONFIG_HOME="$hostile_config" \
XDG_DATA_HOME="$temporary_root/data" \
XDG_STATE_HOME="$temporary_root/state" \
XDG_RUNTIME_DIR="$temporary_root/runtime" \
  scripts/terminal/systemd-user-service.sh --prefix "$prefix" --apply \
  >"$temporary_root/hostile-unit.out" 2>&1 || hostile_status=$?
check "unit installer refuses an unmanaged terminal fragment" \
  test "$hostile_status" -ne 0
check "unit conflict does not overwrite the foreign fragment" \
  contains "$(cat -- "$hostile_unit_dir/agl-terminald.service")" '# foreign unit'
rm -f -- "$hostile_unit_dir/agl-terminald.service"
mkdir "$hostile_unit_dir/agl-terminald.socket.d"
dropin_status=0
HOME="$temporary_root/home" \
XDG_CONFIG_HOME="$hostile_config" \
XDG_DATA_HOME="$temporary_root/data" \
XDG_STATE_HOME="$temporary_root/state" \
XDG_RUNTIME_DIR="$temporary_root/runtime" \
  scripts/terminal/systemd-user-service.sh --prefix "$prefix" --apply \
  >"$temporary_root/hostile-dropin.out" 2>&1 || dropin_status=$?
check "unit installer refuses a terminal drop-in surface" \
  test "$dropin_status" -ne 0

write_binary agl-terminal ui-v2
second_output="$(install_once)"
second_generation="$(sed -n 's/^generation=//p' <<<"$second_output")"
check "UI-only changes publish a distinct generation" \
  test "$first_generation" != "$second_generation"
check "the prior immutable generation is unchanged" \
  equals "$("$first_generation/agl-terminal")" ui-v1

write_binary agl-terminal ui-v3
fault_status=0
AGL_TERMINAL_INSTALL_FAULT_AT=before-current-publish install_once \
  >"$temporary_root/fault.out" 2>"$temporary_root/fault.err" || fault_status=$?
check "fault before current publication is typed and nonzero" \
  test "$fault_status" -ne 0
check "fault before current publication preserves the selected generation" \
  equals "$(readlink -f -- "$prefix/libexec/agl-terminal/current")" "$second_generation"

exec 8>"$prefix/libexec/agl-terminal/.operation.lock"
flock -x 8
lock_status=0
install_once >"$temporary_root/lock.out" 2>"$temporary_root/lock.err" || lock_status=$?
flock -u 8
check "concurrent terminal operation fails instead of waiting or publishing" \
  test "$lock_status" -ne 0
check "concurrent terminal operation leaves current unchanged" \
  equals "$(readlink -f -- "$prefix/libexec/agl-terminal/current")" "$second_generation"

if ((failures > 0)); then
  printf 'agl178-packaging: %s contract checks failed\n' "$failures" >&2
  exit 1
fi
printf 'agl178-packaging: passed\n'
