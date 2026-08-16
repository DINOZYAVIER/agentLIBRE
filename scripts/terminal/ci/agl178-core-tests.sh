#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../.." && pwd)"
cd "$repo_root"

cargo test -p agl-terminal-protocol --test agl178_generation_manifest
cargo test -p agl-terminal-protocol --test agl178_protocol_identity
cargo test -p agl-terminald --test agl178_activation
scripts/terminal/ci/agl178-packaging-verification.sh
