#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"
cd "$repo_root"

if [[ "${AGL_CI_SKIP_PREPARE:-0}" != "1" ]]; then
  "$script_dir/prepare.sh"
fi

echo '==> cargo fmt'
cargo fmt --all -- --check

echo '==> cargo check'
cargo check --locked --workspace

echo '==> cargo clippy'
cargo clippy --locked --workspace --all-targets --no-deps -- -D warnings

echo '==> cargo test'
cargo test --locked --workspace

echo '==> behavior checks'
python3 "$script_dir/check-behavior-tests.py"

echo '==> whitespace'
git diff --check
