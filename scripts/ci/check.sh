#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

if [[ "${AGL_CI_SKIP_PREPARE:-0}" != "1" ]]; then
  "$script_dir/prepare.sh"
fi

"$script_dir/tool-versions.sh"
"$script_dir/metadata.sh"
"$script_dir/fmt.sh"
"$script_dir/clippy.sh"
"$script_dir/test.sh"
"$script_dir/fsm-boundaries.sh"
"$script_dir/systemd-dry-run.sh"
"$script_dir/diff-check.sh"
