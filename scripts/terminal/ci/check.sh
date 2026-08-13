#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../.." && pwd)"
cd "$repo_root"

cargo metadata --locked --no-deps --format-version 1 >/dev/null
cargo fmt --all -- --check
cargo clippy -p agl-exec -p agl-process-launcher -p agl-pty -p agl-terminal \
  -p agl-terminal-client -p agl-terminal-protocol -p agl-terminal-ui \
  -p agl-terminald --lib --bins --locked --no-deps -- -D warnings
cargo test -p agl-exec -p agl-process-launcher -p agl-pty -p agl-terminal \
  -p agl-terminal-client -p agl-terminal-protocol -p agl-terminal-ui \
  -p agl-terminald --lib --bins --locked
python3 scripts/terminal/ci/check-boundaries.py
scripts/terminal/ci/install-smoke.sh
scripts/terminal/ci/systemd-activation-smoke.sh
git diff --check

printf 'agl-terminal-ci: passed\n'
