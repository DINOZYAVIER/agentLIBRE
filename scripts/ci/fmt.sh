#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cargo
ci_need_tool git

ci_cd_repo

ci_section "Rust format"
ci_run cargo fmt --all -- --check

ci_section "constrained llama-server patch whitespace"
ci_run git -C vendor/llama.cpp diff --check HEAD^
