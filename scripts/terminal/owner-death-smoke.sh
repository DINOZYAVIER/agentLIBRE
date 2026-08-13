#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"

[[ "$(uname -s)" == "Linux" ]] || {
  printf 'agl-terminal-owner-death: native acceptance requires Linux\n' >&2
  exit 1
}

cd "$repo_root"
cargo build -p agl-process-launcher --bins --features native-test-fixtures --locked
cargo test \
  -p agl-process-launcher \
  --features native-test-fixtures \
  --test owner_death_native \
  --locked \
  native_owner_death_and_descendant_cleanup_smoke \
  -- \
  --ignored \
  --exact \
  --nocapture

printf 'agl-terminal-owner-death: passed\n'
