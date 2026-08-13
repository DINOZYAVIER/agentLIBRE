#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../.." && pwd)"
cd "$repo_root"

scripts/terminal/ci/check.sh
scripts/terminal/native-linux-smoke.sh
scripts/terminal/owner-death-smoke.sh

printf 'agl-terminal-reintegration: passed\n'
