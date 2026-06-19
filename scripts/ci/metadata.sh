#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cargo

ci_cd_repo

ci_section "Cargo metadata"
ci_run cargo metadata --locked --no-deps --format-version 1
