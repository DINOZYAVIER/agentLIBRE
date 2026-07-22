#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_cd_repo
ci_need_tool flock
ci_need_tool cc

tmp_dir="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$tmp_dir" 2>/dev/null || true
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

install_root="$tmp_dir/install-plan"
output="$(
  scripts/install-agl-cargo.sh \
    --dry-run \
    --root "$install_root" \
    --skip-submodules \
    --skip-llama-build
)"

launcher_command="--path $AGL_CI_REPO_ROOT/crates/agl-process --bin agl-process-launcher"
worker_command="--path $AGL_CI_REPO_ROOT/crates/agl-inference-worker --bin agl-inference-worker"
agl_command="--path $AGL_CI_REPO_ROOT/crates/agl-cli --bin agl"
stage_root="$install_root/libexec/agentlibre/generations/.staging.DRY-RUN/.cargo-root"
[[ "$output" == *"$launcher_command"* ]] ||
  ci_fail "install plan omitted the process launcher: $output"
[[ "$output" == *"$worker_command"* ]] ||
  ci_fail "install plan omitted the private inference worker: $output"
[[ "$output" == *"$agl_command"* ]] ||
  ci_fail "install plan omitted agl: $output"
[[ "$output" == *"--root $stage_root"* ]] ||
  ci_fail "install plan did not stage all runtime binaries below the selected root: $output"
[[ "$output" == *"$AGL_CI_REPO_ROOT/target/release/agl-inference-native"* ]] ||
  ci_fail "install plan omitted the exact native inference bundle: $output"
[[ "$output" == *"pin exact Nix runtime references below final generation .nix-gc-roots"* ]] ||
  ci_fail "install plan omitted Nix GC portability for external ELF references: $output"
[[ "$output" == *"publish complete generation through $install_root/libexec/agentlibre/current"* ]] ||
  ci_fail "install plan omitted the atomic current publication: $output"

launcher_line="$(printf '%s\n' "$output" | grep -n -F -- "$launcher_command" | cut -d: -f1)"
worker_line="$(printf '%s\n' "$output" | grep -n -F -- "$worker_command" | cut -d: -f1)"
agl_line="$(printf '%s\n' "$output" | grep -n -F -- "$agl_command" | cut -d: -f1)"
[[ "$launcher_line" -lt "$worker_line" && "$worker_line" -lt "$agl_line" ]] ||
  ci_fail "install plan must stage the launcher and worker before agl: $output"

default_root="$tmp_dir/default-cargo-home"
default_output="$(
  env \
    HOME="$tmp_dir/home" \
    CARGO_HOME="$default_root" \
    CARGO_INSTALL_ROOT= \
    scripts/install-agl-cargo.sh \
      --dry-run \
      --skip-submodules \
      --skip-llama-build
)"
[[ "$default_output" == *"--root $default_root/libexec/agentlibre/generations/.staging.DRY-RUN/.cargo-root"* ]] ||
  ci_fail "install plan did not resolve an explicit default root: $default_output"

fake_bin="$tmp_dir/fake-bin"
llama_bin="$tmp_dir/llama/bin"
fake_target="$tmp_dir/fake-target"
fake_bundle_digest="$(printf 'a%.0s' {1..64})"
fake_native_bundle="$fake_target/release/agl-inference-native/sha256-$fake_bundle_digest"
fake_other_bundle="$fake_target/release/agl-inference-native/sha256-$(printf 'c%.0s' {1..64})"
case "$(uname -m)" in
  x86_64) fake_dynamic_linker="/lib64/ld-linux-x86-64.so.2" ;;
  aarch64) fake_dynamic_linker="/lib/ld-linux-aarch64.so.1" ;;
  *) ci_fail "unsupported fixture architecture: $(uname -m)" ;;
esac
mkdir -p "$fake_bin" "$llama_bin" "$fake_native_bundle" "$fake_other_bundle"
printf 'unselected build variant\n' >"$fake_other_bundle/not-a-runtime-library"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'root=""' \
  'bin=""' \
  'while [[ $# -gt 0 ]]; do' \
  '  case "$1" in' \
  '    --root) root="$2"; shift 2 ;;' \
  '    --bin) bin="$2"; shift 2 ;;' \
  '    *) shift ;;' \
  '  esac' \
  'done' \
  '[[ -n "$root" && -n "$bin" ]]' \
  'mkdir -p "$root/bin"' \
  'label="${FAKE_BUNDLE_LABEL:-new}"' \
  'if [[ "$bin" == "agl-inference-worker" ]]; then' \
  '  source_file="$CARGO_TARGET_DIR/fake-inference-worker.c"' \
  '  compiler="${CC:-cc}"' \
  '  nix_rpath_guard="NIX_DONT_SET_RPATH_$("$compiler" -dumpmachine | tr - _)"' \
  '  printf '\''#include <stdio.h>\nint main(void) { puts("%s"); return 0; }\n'\'' "$label" >"$source_file"' \
  '  env "$nix_rpath_guard=1" "$compiler" "$source_file" -Wl,--dynamic-linker,"${FAKE_DYNAMIC_LINKER:?}" -Wl,-rpath,"\$ORIGIN/agl-inference-native/sha256-${FAKE_BUNDLE_DIGEST:?}" -o "$root/bin/$bin"' \
  'else' \
  '  printf '\''#!/usr/bin/env bash\nif [[ "${AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE:-}" == "1" && "${FAKE_IDENTITY_FAIL:-}" == "1" ]]; then exit 43; fi\nprintf "%%s\\n" "%s"\n'\'' "$label" >"$root/bin/$bin"' \
  '  chmod 0755 "$root/bin/$bin"' \
  'fi' \
  'alias_path="$CARGO_TARGET_DIR/fake-profile-alias-$bin"' \
  'rm -f -- "$alias_path"' \
  'ln -- "$root/bin/$bin" "$alias_path"' \
  'if [[ "${FAKE_CARGO_FAIL_BIN:-}" == "$bin" ]]; then' \
  '  exit "${FAKE_CARGO_FAIL_STATUS:-42}"' \
  'fi' \
  >"$fake_bin/cargo"
chmod 0755 "$fake_bin/cargo"

for library in \
  libllama-common.so \
  libllama.so \
  libggml.so \
  libggml-base.so \
  libggml-cpu-test.so
do
  : >"$llama_bin/$library"
done

for library in \
  libllama-common.so.0 \
  libmtmd.so.0 \
  libllama.so.0 \
  libggml.so.0 \
  libggml-base.so.0 \
  libggml-cpu-test.so
do
  : >"$fake_native_bundle/$library"
  chmod 0555 "$fake_native_bundle/$library"
done
chmod 0555 "$fake_native_bundle"

write_bundle_binary() {
  local path="$1"
  local label="$2"
  printf '#!/usr/bin/env bash\nprintf "%%s\\n" "%s"\n' "$label" >"$path"
  chmod 0755 "$path"
}

seed_regular_pair() {
  local root="$1"
  local label="$2"
  mkdir -p "$root/bin"
  write_bundle_binary "$root/bin/agl" "$label"
  write_bundle_binary "$root/bin/agl-process-launcher" "$label"
}

run_fake_installer() {
  local root="$1"
  shift
  env \
    PATH="$fake_bin:$PATH" \
    HOME="$tmp_dir/home" \
    XDG_STATE_HOME="$tmp_dir/state" \
    AGL_LLAMA_CPP_BUILD_DIR="$tmp_dir/llama" \
    FAKE_BUNDLE_DIGEST="$fake_bundle_digest" \
    FAKE_DYNAMIC_LINKER="$fake_dynamic_linker" \
    CARGO_TARGET_DIR="$fake_target" \
    "$@" \
    scripts/install-agl-cargo.sh \
      --root "$root" \
      --skip-submodules \
      --skip-llama-build
}

assert_current_complete() {
  local root="$1"
  local expected="$2"
  local current="$root/libexec/agentlibre/current"
  local generation
  local agl_label
  local launcher_label
  local worker_label

  [[ -L "$current" ]] || ci_fail "missing atomic current pointer under $root"
  generation="$(readlink -f -- "$current")"
  [[ -x "$generation/agl" ]] || ci_fail "current generation has no agl under $root"
  [[ -x "$generation/agl-process-launcher" ]] ||
    ci_fail "current generation has no launcher under $root"
  [[ -x "$generation/agl-inference-worker" ]] ||
    ci_fail "current generation has no private inference worker under $root"
  for executable in \
    "$generation/agl" \
    "$generation/agl-process-launcher" \
    "$generation/agl-inference-worker"
  do
    [[ "$(stat -c '%a' -- "$executable")" == 555 &&
      "$(stat -c '%h' -- "$executable")" == 1 ]] ||
      ci_fail "current generation executable retained a writable build-tree alias: $executable"
  done
  [[ -d "$generation/agl-inference-native" && ! -L "$generation/agl-inference-native" ]] ||
    ci_fail "current generation has no exact native inference bundle under $root"
  [[ "$(stat -c '%a' -- "$generation/agl-inference-native")" == 555 ]] ||
    ci_fail "current generation native inference bundle is mutable under $root"
  local published_leaf_count
  published_leaf_count="$(
    find "$generation/agl-inference-native" -mindepth 1 -maxdepth 1 -type d -printf . |
      wc -c
  )"
  [[ "$published_leaf_count" -eq 1 ]] ||
    ci_fail "current generation published an unselected native build variant under $root"
  local selected_native_bundle="$generation/agl-inference-native/sha256-$fake_bundle_digest"
  [[ -d "$selected_native_bundle" && ! -L "$selected_native_bundle" &&
    "$(stat -c '%a' -- "$selected_native_bundle")" == 555 ]] ||
    ci_fail "current generation has no sealed content-addressed native inference leaf under $root"
  [[ -f "$selected_native_bundle/libggml-cpu-test.so" ]] ||
    ci_fail "current generation native inference bundle has no CPU backend under $root"
  agl_label="$("$generation/agl" --version)"
  launcher_label="$("$generation/agl-process-launcher")"
  worker_label="$("$generation/agl-inference-worker")"
  [[ "$agl_label" == "$expected" && "$launcher_label" == "$expected" &&
    "$worker_label" == "$expected" ]] ||
    ci_fail "current resolved an incomplete or mixed bundle under $root: $agl_label/$launcher_label/$worker_label"
}

assert_surface_label() {
  local root="$1"
  local expected="$2"
  local agl_label
  local launcher_label

  agl_label="$("$root/bin/agl" --version)"
  launcher_label="$("$root/bin/agl-process-launcher")"
  [[ "$agl_label" == "$expected" && "$launcher_label" == "$expected" ]] ||
    ci_fail "public commands resolved an incomplete or mixed pair under $root: $agl_label/$launcher_label"
  [[ ! -e "$root/bin/agl-inference-worker" && ! -L "$root/bin/agl-inference-worker" ]] ||
    ci_fail "private inference worker leaked onto the public command surface under $root"
}

assert_surface_runnable_count() {
  local root="$1"
  local expected="$2"
  local count=0

  if [[ -x "$root/bin/agl" ]]; then
    count=$((count + 1))
  fi
  if [[ -x "$root/bin/agl-process-launcher" ]]; then
    count=$((count + 1))
  fi
  [[ ! -e "$root/bin/agl-inference-worker" && ! -L "$root/bin/agl-inference-worker" ]] ||
    ci_fail "private inference worker leaked onto the public command surface under $root"
  [[ "$count" -eq "$expected" ]] ||
    ci_fail "expected $expected runnable public commands under $root, found $count"
}

assert_stable_links() {
  local root="$1"
  [[ -L "$root/bin/agl" ]] || ci_fail "agl is not a stable managed symlink under $root"
  [[ "$(readlink -- "$root/bin/agl")" == "../libexec/agentlibre/current/agl" ]] ||
    ci_fail "agl does not route through the current pointer under $root"
  [[ -L "$root/bin/agl-process-launcher" ]] ||
    ci_fail "process launcher is not a stable managed symlink under $root"
  [[ "$(readlink -- "$root/bin/agl-process-launcher")" == "../libexec/agentlibre/current/agl-process-launcher" ]] ||
    ci_fail "process launcher does not route through the current pointer under $root"
  [[ ! -e "$root/bin/agl-inference-worker" && ! -L "$root/bin/agl-inference-worker" ]] ||
    ci_fail "private inference worker has a public managed link under $root"
}

assert_managed_ancestor_security() {
  local root="$1"
  local path
  local mode
  local expected_uid

  expected_uid="$(id -u)"
  for path in \
    "$root" \
    "$root/bin" \
    "$root/libexec" \
    "$root/libexec/agentlibre" \
    "$root/libexec/agentlibre/generations"
  do
    [[ -d "$path" && ! -L "$path" ]] ||
      ci_fail "managed ancestor is not a real directory: $path"
    [[ "$(stat -c '%u' -- "$path")" == "$expected_uid" ]] ||
      ci_fail "managed ancestor has the wrong owner: $path"
    mode="$(stat -c '%a' -- "$path")"
    [[ "$mode" == 755 ]] ||
      ci_fail "new managed ancestor was not created deterministically as 0755: $path (mode $mode)"
  done
}

assert_no_staging_transaction() {
  local root="$1"
  if compgen -G "$root/libexec/agentlibre/generations/.staging.*" >/dev/null; then
    ci_fail "ordinary staging failure left its private transaction behind under $root"
  fi
}

for install_state in fresh update; do
  failure_root="$tmp_dir/staging-failure-$install_state"
  if [[ "$install_state" == update ]]; then
    run_fake_installer "$failure_root" FAKE_BUNDLE_LABEL=old >"$tmp_dir/staging-seed.out"
  fi
  failure_status=0
  run_fake_installer "$failure_root" \
    FAKE_BUNDLE_LABEL=new \
    FAKE_CARGO_FAIL_BIN=agl \
    >"$tmp_dir/staging-failure-$install_state.out" \
    2>"$tmp_dir/staging-failure-$install_state.err" || failure_status=$?
  [[ "$failure_status" -eq 42 ]] ||
    ci_fail "expected $install_state staged install to fail with 42, got $failure_status"
  if [[ "$install_state" == fresh ]]; then
    assert_surface_runnable_count "$failure_root" 0
    [[ ! -e "$failure_root/libexec/agentlibre/current" &&
      ! -L "$failure_root/libexec/agentlibre/current" ]] ||
      ci_fail "failed fresh staging unexpectedly published current"
  else
    assert_current_complete "$failure_root" old
    assert_surface_label "$failure_root" old
  fi
  assert_no_staging_transaction "$failure_root"
done

for install_state in fresh update; do
  identity_failure_root="$tmp_dir/identity-failure-$install_state"
  if [[ "$install_state" == update ]]; then
    run_fake_installer "$identity_failure_root" FAKE_BUNDLE_LABEL=old >"$tmp_dir/identity-seed.out"
  fi
  identity_failure_status=0
  run_fake_installer "$identity_failure_root" \
    FAKE_BUNDLE_LABEL=new \
    FAKE_IDENTITY_FAIL=1 \
    >"$tmp_dir/identity-failure-$install_state.out" \
    2>"$tmp_dir/identity-failure-$install_state.err" || identity_failure_status=$?
  [[ "$identity_failure_status" -eq 43 ]] ||
    ci_fail "expected $install_state staged identity validation to fail with 43, got $identity_failure_status"
  if [[ "$install_state" == fresh ]]; then
    assert_surface_runnable_count "$identity_failure_root" 0
    [[ ! -e "$identity_failure_root/libexec/agentlibre/current" &&
      ! -L "$identity_failure_root/libexec/agentlibre/current" ]] ||
      ci_fail "failed fresh identity validation unexpectedly published current"
  else
    assert_current_complete "$identity_failure_root" old
    assert_surface_label "$identity_failure_root" old
  fi
  assert_no_staging_transaction "$identity_failure_root"
done

unmanaged_root="$tmp_dir/unmanaged-regular"
seed_regular_pair "$unmanaged_root" old
unmanaged_status=0
run_fake_installer "$unmanaged_root" FAKE_BUNDLE_LABEL=new \
  >"$tmp_dir/unmanaged.out" \
  2>"$tmp_dir/unmanaged.err" || unmanaged_status=$?
[[ "$unmanaged_status" -eq 1 ]] ||
  ci_fail "expected unmanaged regular commands to be rejected"
grep -F "agentLIBRE alpha installs do not migrate flat binaries" "$tmp_dir/unmanaged.err" >/dev/null ||
  ci_fail "unmanaged regular-command rejection was not actionable"
[[ -f "$unmanaged_root/bin/agl" && ! -L "$unmanaged_root/bin/agl" ]] ||
  ci_fail "unmanaged agl was mutated"
[[ -f "$unmanaged_root/bin/agl-process-launcher" &&
  ! -L "$unmanaged_root/bin/agl-process-launcher" ]] ||
  ci_fail "unmanaged launcher was mutated"
assert_surface_label "$unmanaged_root" old
[[ ! -e "$unmanaged_root/libexec/agentlibre/current" &&
  ! -L "$unmanaged_root/libexec/agentlibre/current" ]] ||
  ci_fail "unmanaged rejection unexpectedly published current"
[[ -z "$(find "$unmanaged_root/libexec/agentlibre/generations" -mindepth 1 -print -quit)" ]] ||
  ci_fail "unmanaged rejection unexpectedly staged a generation"

public_worker_root="$tmp_dir/public-worker"
run_fake_installer "$public_worker_root" FAKE_BUNDLE_LABEL=old >"$tmp_dir/public-worker-seed.out"
write_bundle_binary "$public_worker_root/bin/agl-inference-worker" leaked
public_worker_status=0
run_fake_installer "$public_worker_root" FAKE_BUNDLE_LABEL=new \
  >"$tmp_dir/public-worker.out" \
  2>"$tmp_dir/public-worker.err" || public_worker_status=$?
[[ "$public_worker_status" -eq 1 ]] ||
  ci_fail "installer accepted a public inference worker command"
grep -F "refusing a public inference worker command" "$tmp_dir/public-worker.err" >/dev/null ||
  ci_fail "public inference worker rejection was not actionable"
rm -f -- "$public_worker_root/bin/agl-inference-worker"
assert_current_complete "$public_worker_root" old
assert_surface_label "$public_worker_root" old

symlink_root="$tmp_dir/symlink-root"
symlink_root_outside="$tmp_dir/symlink-root-outside"
mkdir -p "$symlink_root_outside"
ln -s -- "$symlink_root_outside" "$symlink_root"
symlink_root_status=0
run_fake_installer "$symlink_root" FAKE_BUNDLE_LABEL=new \
  >"$tmp_dir/symlink-root.out" \
  2>"$tmp_dir/symlink-root.err" || symlink_root_status=$?
[[ "$symlink_root_status" -eq 1 ]] ||
  ci_fail "expected a symlinked install root to be rejected"
grep -F "install root whose path traverses a symlink" "$tmp_dir/symlink-root.err" >/dev/null ||
  ci_fail "symlinked install root rejection was not actionable"
[[ -z "$(find "$symlink_root_outside" -mindepth 1 -print -quit)" ]] ||
  ci_fail "symlinked install root mutated its external target"

symlink_bin_root="$tmp_dir/symlink-bin-root"
symlink_bin_outside="$tmp_dir/symlink-bin-outside"
mkdir -p "$symlink_bin_root" "$symlink_bin_outside"
ln -s -- "$symlink_bin_outside" "$symlink_bin_root/bin"
symlink_bin_status=0
run_fake_installer "$symlink_bin_root" FAKE_BUNDLE_LABEL=new \
  >"$tmp_dir/symlink-bin.out" \
  2>"$tmp_dir/symlink-bin.err" || symlink_bin_status=$?
[[ "$symlink_bin_status" -eq 1 ]] ||
  ci_fail "expected a symlinked install bin directory to be rejected"
grep -F "refusing to use symlinked install bin directory" "$tmp_dir/symlink-bin.err" >/dev/null ||
  ci_fail "symlinked install bin rejection was not actionable"
[[ ! -e "$symlink_bin_outside/agl" && ! -e "$symlink_bin_outside/agl-process-launcher" ]] ||
  ci_fail "symlinked install bin mutated its external target"

symlink_generations_root="$tmp_dir/symlink-generations-root"
symlink_generations_outside="$tmp_dir/symlink-generations-outside"
mkdir -p \
  "$symlink_generations_root/bin" \
  "$symlink_generations_root/libexec/agentlibre" \
  "$symlink_generations_outside"
ln -s -- "$symlink_generations_outside" \
  "$symlink_generations_root/libexec/agentlibre/generations"
symlink_generations_status=0
run_fake_installer "$symlink_generations_root" FAKE_BUNDLE_LABEL=new \
  >"$tmp_dir/symlink-generations.out" \
  2>"$tmp_dir/symlink-generations.err" || symlink_generations_status=$?
[[ "$symlink_generations_status" -eq 1 ]] ||
  ci_fail "expected a symlinked generations directory to be rejected"
grep -F "refusing to use symlinked runtime generations directory" \
  "$tmp_dir/symlink-generations.err" >/dev/null ||
  ci_fail "symlinked generations rejection was not actionable"
[[ -z "$(find "$symlink_generations_outside" -mindepth 1 -print -quit)" ]] ||
  ci_fail "symlinked generations directory mutated its external target"

current_escape_root="$tmp_dir/current-escape-root"
current_escape_outside="$tmp_dir/current-escape-outside"
mkdir -p \
  "$current_escape_root/bin" \
  "$current_escape_root/libexec/agentlibre/generations" \
  "$current_escape_outside"
write_bundle_binary "$current_escape_outside/agl" outside
write_bundle_binary "$current_escape_outside/agl-process-launcher" outside
ln -s -- "../libexec/agentlibre/current/agl" "$current_escape_root/bin/agl"
ln -s -- "../libexec/agentlibre/current/agl-process-launcher" \
  "$current_escape_root/bin/agl-process-launcher"
ln -s -- "$current_escape_outside" "$current_escape_root/libexec/agentlibre/current"
current_escape_status=0
run_fake_installer "$current_escape_root" FAKE_BUNDLE_LABEL=new \
  >"$tmp_dir/current-escape.out" \
  2>"$tmp_dir/current-escape.err" || current_escape_status=$?
[[ "$current_escape_status" -eq 1 ]] ||
  ci_fail "expected an escaping current pointer to be rejected"
grep -F "current pointer escapes the generations directory" "$tmp_dir/current-escape.err" >/dev/null ||
  ci_fail "escaping current-pointer rejection was not actionable"
assert_surface_label "$current_escape_root" outside
[[ -z "$(find "$current_escape_root/libexec/agentlibre/generations" -mindepth 1 -print -quit)" ]] ||
  ci_fail "escaping current-pointer rejection unexpectedly staged a generation"

umask_root="$tmp_dir/umask-zero"
(umask 000; run_fake_installer "$umask_root" FAKE_BUNDLE_LABEL=secure \
  >"$tmp_dir/umask-zero.out" 2>"$tmp_dir/umask-zero.err")
assert_managed_ancestor_security "$umask_root"
assert_current_complete "$umask_root" secure
assert_surface_label "$umask_root" secure

writable_ancestor_root="$tmp_dir/writable-ancestor"
mkdir -p "$writable_ancestor_root/bin" "$writable_ancestor_root/libexec"
chmod 0755 "$writable_ancestor_root" "$writable_ancestor_root/bin"
chmod 0775 "$writable_ancestor_root/libexec"
writable_ancestor_status=0
run_fake_installer "$writable_ancestor_root" FAKE_BUNDLE_LABEL=new \
  >"$tmp_dir/writable-ancestor.out" \
  2>"$tmp_dir/writable-ancestor.err" || writable_ancestor_status=$?
[[ "$writable_ancestor_status" -eq 1 ]] ||
  ci_fail "expected a group-writable managed ancestor to be rejected"
grep -F "refusing group/other-writable libexec directory" \
  "$tmp_dir/writable-ancestor.err" >/dev/null ||
  ci_fail "group-writable managed ancestor rejection was not actionable"
[[ "$(stat -c '%a' -- "$writable_ancestor_root/libexec")" == 775 ]] ||
  ci_fail "writable managed ancestor rejection mutated the unsafe directory"
[[ ! -e "$writable_ancestor_root/libexec/agentlibre" ]] ||
  ci_fail "writable managed ancestor rejection continued into the runtime layout"

publish_root="$tmp_dir/publish"
run_fake_installer "$publish_root" FAKE_BUNDLE_LABEL=old >"$tmp_dir/publish-old.out"
assert_stable_links "$publish_root"
assert_current_complete "$publish_root" old
assert_surface_label "$publish_root" old
old_generation="$(readlink -f -- "$publish_root/libexec/agentlibre/current")"
run_fake_installer "$publish_root" FAKE_BUNDLE_LABEL=new >"$tmp_dir/publish-new.out"
assert_stable_links "$publish_root"
assert_current_complete "$publish_root" new
assert_surface_label "$publish_root" new
[[ -x "$old_generation/agl" && -x "$old_generation/agl-process-launcher" &&
  -x "$old_generation/agl-inference-worker" ]] ||
  ci_fail "managed update did not retain its prior immutable generation"
[[ "$($old_generation/agl --version)" == old &&
  "$($old_generation/agl-process-launcher)" == old &&
  "$($old_generation/agl-inference-worker)" == old ]] ||
  ci_fail "managed update mutated its prior immutable generation"
[[ "$(stat -c '%a' -- "$publish_root/.agentlibre-runtime.lock")" == 600 ]] ||
  ci_fail "runtime bundle lock is not private"

declare -A fresh_fault_runnable=(
  [after-generation-ready]=0
  [before-initial-links]=0
  [after-initial-launcher-link]=0
  [after-initial-agl-link]=0
  [before-initial-current-publish]=0
  [after-initial-current-publish]=2
)

for fault in \
  after-generation-ready \
  before-initial-links \
  after-initial-launcher-link \
  after-initial-agl-link \
  before-initial-current-publish \
  after-initial-current-publish
do
  fault_root="$tmp_dir/fresh-fault-$fault"
  fault_status=0
  run_fake_installer "$fault_root" \
    FAKE_BUNDLE_LABEL=new \
    AGL_INSTALL_FAULT_AT="$fault" \
    >"$tmp_dir/$fault.out" \
    2>"$tmp_dir/$fault.err" || fault_status=$?
  [[ "$fault_status" -ne 0 ]] || ci_fail "fault point $fault did not interrupt installation"
  assert_surface_runnable_count "$fault_root" "${fresh_fault_runnable[$fault]}"
  [[ "${fresh_fault_runnable[$fault]}" -ne 1 ]] ||
    ci_fail "fresh fault point $fault left exactly one runnable public command"
  if [[ -L "$fault_root/libexec/agentlibre/current" ]]; then
    assert_current_complete "$fault_root" new
  fi
  run_fake_installer "$fault_root" FAKE_BUNDLE_LABEL=recovered >"$tmp_dir/$fault-retry.out"
  assert_current_complete "$fault_root" recovered
  assert_surface_label "$fault_root" recovered
done

declare -A update_fault_expected=(
  [after-generation-ready]=old
  [before-new-current-publish]=old
  [after-new-current-publish]=new
)

for fault in after-generation-ready before-new-current-publish after-new-current-publish; do
  fault_root="$tmp_dir/update-fault-$fault"
  run_fake_installer "$fault_root" FAKE_BUNDLE_LABEL=old >"$tmp_dir/update-$fault-seed.out"
  fault_status=0
  run_fake_installer "$fault_root" \
    FAKE_BUNDLE_LABEL=new \
    AGL_INSTALL_FAULT_AT="$fault" \
    >"$tmp_dir/update-$fault.out" \
    2>"$tmp_dir/update-$fault.err" || fault_status=$?
  [[ "$fault_status" -ne 0 ]] || ci_fail "update fault point $fault did not interrupt installation"
  assert_surface_runnable_count "$fault_root" 2
  assert_current_complete "$fault_root" "${update_fault_expected[$fault]}"
  assert_surface_label "$fault_root" "${update_fault_expected[$fault]}"
  run_fake_installer "$fault_root" FAKE_BUNDLE_LABEL=recovered >"$tmp_dir/update-$fault-retry.out"
  assert_current_complete "$fault_root" recovered
  assert_surface_label "$fault_root" recovered
done

exec {held_lock_fd}<>"$publish_root/.agentlibre-runtime.lock"
flock --exclusive "$held_lock_fd"
locked_status=0
run_fake_installer "$publish_root" FAKE_BUNDLE_LABEL=locked \
  >"$tmp_dir/locked.out" \
  2>"$tmp_dir/locked.err" || locked_status=$?
flock --unlock "$held_lock_fd"
exec {held_lock_fd}>&-
[[ "$locked_status" -eq 1 ]] ||
  ci_fail "expected a concurrent bundle lock to reject installation, got $locked_status"
grep -F "another runtime bundle operation holds" "$tmp_dir/locked.err" >/dev/null ||
  ci_fail "concurrent bundle lock rejection was not actionable"
assert_current_complete "$publish_root" new
assert_surface_label "$publish_root" new

printf '%s\n' "$output"
echo "install immutable runtime bundle dry-run: passed"
