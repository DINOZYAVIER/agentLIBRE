#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"
ci_cd_repo

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
    printf 'agl178-install: FAIL: %s\n' "$label" >&2
    failures=$((failures + 1))
  fi
}
contains() {
  [[ "$1" == *"$2"* ]]
}

terminal_revision="$(git rev-parse --verify HEAD^{commit})"

generation="$temporary_root/terminal generation"
mkdir -p "$generation"
for entry in agl-terminald agl-process-launcher agl-terminal; do
  printf '%s fixture\n' "$entry" >"$generation/$entry"
  chmod 0555 "$generation/$entry"
done

file_entry() {
  local role="$1"
  local path="$2"
  local size
  local digest
  size="$(stat -c '%s' -- "$generation/$path")"
  digest="sha256:$(sha256sum -- "$generation/$path" | awk '{print $1}')"
  printf '    {"role":"%s","path":"%s","byte_size":%s,"sha256":"%s"}' \
    "$role" "$path" "$size" "$digest"
}
{
  printf '{\n'
  printf '  "schema":"agl-terminal.runtime-generation.v2",\n'
  printf '  "product_version":"1.0.0-alpha.1",\n'
  printf '  "protocol_version":2,\n'
  printf '  "source_revision":"%s",\n' "$terminal_revision"
  printf '  "files":[\n'
  file_entry service agl-terminald
  printf ',\n'
  file_entry launcher agl-process-launcher
  printf ',\n'
  file_entry ui agl-terminal
  printf '\n  ]\n}\n'
} >"$generation/runtime-manifest.json"
chmod 0444 "$generation/runtime-manifest.json"
chmod 0555 "$generation"
manifest_digest="$(sha256sum -- "$generation/runtime-manifest.json" | awk '{print $1}')"
mkdir -p "$temporary_root/terminal/generations"
canonical_generation="$temporary_root/terminal/generations/generation-$manifest_digest"
mkdir "$canonical_generation"
cp -a -- "$generation/." "$canonical_generation/"
chmod 0555 "$canonical_generation"
chmod 0755 "$generation"
rm -rf -- "$generation"
generation="$canonical_generation"

help="$(scripts/install-agl-cargo.sh --help)"
check "help exposes required exact generation input" \
  contains "$help" '--terminal-generation DIR'
check "help has no prefix-based terminal selector" \
  test "$(grep -c -- '--terminal-prefix' <<<"$help" || true)" -eq 0

missing_status=0
missing_output="$(
  scripts/install-agl-cargo.sh --dry-run --root "$temporary_root/agent-missing" \
    --skip-submodules --skip-llama-build 2>&1
)" || missing_status=$?
check "terminal generation argument is mandatory" test "$missing_status" -ne 0
check "missing argument has an actionable diagnostic" \
  contains "$missing_output" '--terminal-generation DIR is required'

selected_status=0
selected_output="$(
  scripts/install-agl-cargo.sh --dry-run --root "$temporary_root/agent" \
    --terminal-generation "$generation" --skip-submodules --skip-llama-build 2>&1
)" || selected_status=$?
check "canonical exact generation is accepted as install input" test "$selected_status" -eq 0
check "dry-run names offline terminal verification" \
  contains "$selected_output" "verify exact terminal generation $generation offline"
check "dry-run seals the verified terminal manifest identity" \
  contains "$selected_output" 'seal terminal manifest digest into agent runtime-manifest v3'

chmod 0755 "$generation"
ln -s "$generation" "$temporary_root/terminal-generation-link"
symlink_status=0
scripts/install-agl-cargo.sh --dry-run --root "$temporary_root/agent-link" \
  --terminal-generation "$temporary_root/terminal-generation-link" \
  --skip-submodules --skip-llama-build >/dev/null 2>&1 || symlink_status=$?
check "symlink generation input is rejected" test "$symlink_status" -ne 0

prefix_status=0
scripts/install-agl-cargo.sh --dry-run --root "$temporary_root/agent-prefix" \
  --terminal-prefix "$temporary_root" --skip-submodules --skip-llama-build \
  >/dev/null 2>&1 || prefix_status=$?
check "prefix selector is not a compatibility path" test "$prefix_status" -ne 0

if ((failures > 0)); then
  printf 'agl178-install: %s contract checks failed\n' "$failures" >&2
  exit 1
fi
printf 'agl178-install: passed\n'
