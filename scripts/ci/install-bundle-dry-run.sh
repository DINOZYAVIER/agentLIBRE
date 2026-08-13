#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_cd_repo
ci_need_tool flock

tmp_dir="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$tmp_dir" 2>/dev/null || true
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

terminal_stage="$tmp_dir/terminal-stage"
mkdir -p "$terminal_stage"
for entry in agl-terminald agl-process-launcher agl-terminal; do
  printf '%s fixture\n' "$entry" >"$terminal_stage/$entry"
  chmod 0555 "$terminal_stage/$entry"
done
terminal_file_entry() {
  local role="$1"
  local path="$2"
  printf '    {"role":"%s","path":"%s","byte_size":%s,"sha256":"sha256:%s"}' \
    "$role" "$path" \
    "$(stat -c '%s' -- "$terminal_stage/$path")" \
    "$(sha256sum -- "$terminal_stage/$path" | awk '{print $1}')"
}
{
  printf '{"schema":"agl-terminal.runtime-generation.v2","product_version":"1.0.0-alpha.1","source_revision":"%s","protocol_version":2,"files":[\n' \
    "$(git rev-parse HEAD)"
  terminal_file_entry service agl-terminald
  printf ',\n'
  terminal_file_entry launcher agl-process-launcher
  printf ',\n'
  terminal_file_entry ui agl-terminal
  printf '\n]}\n'
} >"$terminal_stage/runtime-manifest.json"
chmod 0444 "$terminal_stage/runtime-manifest.json"
chmod 0555 "$terminal_stage"
terminal_digest="$(sha256sum -- "$terminal_stage/runtime-manifest.json" | awk '{print $1}')"
terminal_generation="$tmp_dir/terminal/generations/generation-$terminal_digest"
mkdir -p "$(dirname -- "$terminal_generation")"
mkdir "$terminal_generation"
cp -a -- "$terminal_stage/." "$terminal_generation/"
chmod 0555 "$terminal_generation"

install_root="$tmp_dir/install-plan"
output="$(
  AGL_LLAMA_CPP_BUILD_DIR="$tmp_dir/llama" \
    scripts/install-agl-cargo.sh --dry-run --root "$install_root" \
      --terminal-generation "$terminal_generation" \
      --skip-submodules --skip-llama-build
)"
agl_command="--path $AGL_CI_REPO_ROOT/crates/agl-cli --bin agl"
[[ "$output" == *"$agl_command"* ]] || ci_fail "install plan omitted agl: $output"
[[ "$output" == *"copy private llama-server and exact lib*.so* closure"* ]] ||
  ci_fail "install plan omitted the selected engine closure: $output"
[[ "$output" != *"agl-inference-worker"* && "$output" != *"agl-inference-native"* ]] ||
  ci_fail "install plan retained the deleted worker/native bundle: $output"
[[ "$output" == *"publish complete generation through $install_root/libexec/agentlibre/current"* ]] ||
  ci_fail "install plan omitted atomic generation publication: $output"

fake_bin="$tmp_dir/fake-bin"
llama_bin="$tmp_dir/llama/bin"
mkdir -p "$fake_bin" "$llama_bin"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'root=""' \
  'while [[ $# -gt 0 ]]; do' \
  '  case "$1" in --root) root="$2"; shift 2 ;; *) shift ;; esac' \
  'done' \
  '[[ -n "$root" ]]' \
  'mkdir -p "$root/bin"' \
  'label="${FAKE_BUNDLE_LABEL:-new}"' \
  'printf '\''#!/usr/bin/env bash\nset -euo pipefail\nif [[ -n "${AGL_INTERNAL_SEAL_RUNTIME_MANIFEST:-}" ]]; then printf "{}\\n" >"$AGL_INTERNAL_SEAL_RUNTIME_MANIFEST/runtime-manifest.json"; chmod 0444 "$AGL_INTERNAL_SEAL_RUNTIME_MANIFEST/runtime-manifest.json"; exit 0; fi\nprintf "%%s\\n" "%s"\n'\'' "$label" >"$root/bin/agl"' \
  'chmod 0755 "$root/bin/agl"' \
  >"$fake_bin/cargo"
chmod 0755 "$fake_bin/cargo"

printf '#!/usr/bin/env bash\nprintf "new\\n"\n' >"$llama_bin/llama-server"
chmod 0755 "$llama_bin/llama-server"
for library in libllama-common.so libllama.so libggml.so libggml-base.so \
  libggml-cpu-test.so libllama-server-impl.so; do
  printf 'fixture library\n' >"$llama_bin/$library"
done

run_installer() {
  local root="$1"
  local label="$2"
  shift 2
  printf '#!/usr/bin/env bash\nprintf "%%s\\n" "%s"\n' "$label" >"$llama_bin/llama-server"
  chmod 0755 "$llama_bin/llama-server"
  env \
    PATH="$fake_bin:$PATH" \
    AGL_LLAMA_CPP_BUILD_DIR="$tmp_dir/llama" \
    XDG_CONFIG_HOME="$tmp_dir/xdg" \
    HOME="$tmp_dir/home" \
    FAKE_BUNDLE_LABEL="$label" \
    scripts/install-agl-cargo.sh --root "$root" \
      --terminal-generation "$terminal_generation" \
      --skip-submodules --skip-llama-build "$@"
}

assert_generation() {
  local root="$1"
  local expected="$2"
  local generation
  [[ -L "$root/bin/agl" ]] || ci_fail "agl public surface is not a managed link"
  [[ "$(readlink -- "$root/bin/agl")" == "../libexec/agentlibre/current/agl" ]] ||
    ci_fail "agl public link bypasses the current generation"
  [[ ! -e "$root/bin/llama-server" && ! -L "$root/bin/llama-server" ]] ||
    ci_fail "private llama-server leaked onto the public command surface"
  [[ ! -e "$root/bin/agl-inference-worker" && ! -L "$root/bin/agl-inference-worker" ]] ||
    ci_fail "deleted inference worker leaked onto the public command surface"
  generation="$(readlink -f -- "$root/libexec/agentlibre/current")"
  [[ "$($generation/agl --version)" == "$expected" ]] || ci_fail "wrong agl generation"
  [[ "$($generation/llama-server)" == "$expected" ]] || ci_fail "wrong engine generation"
  for file in "$generation/agl" "$generation/llama-server" \
    "$generation/libllama-server-impl.so"; do
    [[ -f "$file" && ! -L "$file" && "$(stat -c '%a' -- "$file")" == 555 &&
      "$(stat -c '%h' -- "$file")" == 1 ]] ||
      ci_fail "runtime component is not an immutable single-link file: $file"
  done
  [[ -f "$generation/runtime-manifest.json" &&
    "$(stat -c '%a' -- "$generation/runtime-manifest.json")" == 444 ]] ||
    ci_fail "generation has no sealed runtime manifest"
}

root="$tmp_dir/install"
run_installer "$root" old >/dev/null
assert_generation "$root" old
old_generation="$(readlink -f -- "$root/libexec/agentlibre/current")"
run_installer "$root" new >/dev/null
assert_generation "$root" new
[[ "$($old_generation/agl --version)" == old ]] ||
  ci_fail "atomic update mutated the previous immutable generation"

no_force_status=0
run_installer "$root" forbidden --no-force >/dev/null 2>&1 ||
  no_force_status=$?
[[ "$no_force_status" -ne 0 ]] || ci_fail "--no-force replaced an installed runtime"
assert_generation "$root" new

unmanaged="$tmp_dir/unmanaged"
mkdir -p "$unmanaged/bin"
printf '#!/bin/sh\nexit 0\n' >"$unmanaged/bin/agl"
chmod 0755 "$unmanaged/bin/agl"
unmanaged_status=0
run_installer "$unmanaged" forbidden >/dev/null 2>&1 ||
  unmanaged_status=$?
[[ "$unmanaged_status" -ne 0 ]] || ci_fail "installer replaced an unmanaged command"

echo "install bundle tests passed"
