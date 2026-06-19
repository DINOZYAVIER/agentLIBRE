#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cmake
ci_need_tool git

ci_cd_repo
ci_ensure_submodule

export AGL_LLAMA_CPP_BUILD_JOBS="${AGL_LLAMA_CPP_BUILD_JOBS:-$(ci_default_jobs)}"

ci_section "Building llama.cpp native libraries"
ci_run scripts/build-llama-cpp.sh
