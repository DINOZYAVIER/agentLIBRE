#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../.." && pwd)"
cd "$repo_root"

packages=(
  agl-exec
  agl-pty
  agl-terminal
  agl-terminal-protocol
  agl-terminal-client
  agl-process-launcher
  agl-terminald
  agl-terminal-ui
)

for package in "${packages[@]}"; do
  cargo package --locked --allow-dirty --no-verify --list -p "$package" >/dev/null
done

scripts/terminal/install.sh --dry-run
scripts/terminal/uninstall.sh --dry-run
scripts/terminal/ci/install-smoke.sh
scripts/terminal/ci/systemd-activation-smoke.sh
printf 'agl-terminal-release-smoke: passed packages=%s\n' "${#packages[@]}"
