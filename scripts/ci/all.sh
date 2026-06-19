#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_section "Running full CI"
"$script_dir/check.sh"
AGL_CI_SKIP_PREPARE=1 "$script_dir/release-smoke.sh"
