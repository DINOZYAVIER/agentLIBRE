#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cargo
ci_need_tool cmake
ci_need_tool git
ci_need_tool rustc

ci_cd_repo

ci_section "Tool versions"
ci_run rustc --version
ci_run cargo --version
ci_run cmake --version
ci_run git submodule status --recursive
