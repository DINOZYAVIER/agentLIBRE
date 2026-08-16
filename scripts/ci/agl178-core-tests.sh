#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"
cd "$repo_root"

cargo test -p agl-runtime --test agl178_runtime_manifest
cargo test -p agl-runtime --test agl178_install_lifecycle
cargo test -p agl-runtime --test agl178_installed_live
cargo test -p agl-process --test agl178_terminal_endpoint
scripts/ci/agl178-install-verification.sh
scripts/ci/agl178-systemd-verification.sh
