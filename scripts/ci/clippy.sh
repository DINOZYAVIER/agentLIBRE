#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cargo

ci_cd_repo

ci_section "Clippy"
ci_run cargo clippy --locked --workspace --all-targets --no-deps -- -D warnings
