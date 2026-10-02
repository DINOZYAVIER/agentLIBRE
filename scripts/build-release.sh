#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<'USAGE'
Usage: scripts/build-release.sh [--root PATH] [--no-services] [--dry-run]

Build the locked Rust workspace and private Vulkan llama.cpp engine, then
replace the installed runtime and user-systemd units and restart the services.
The Matrix bridge is configured when its config file exists.

Options:
  --root PATH      installation prefix (default: Cargo install root)
  --no-services    install binaries only; leave systemd units/services untouched
  --dry-run        print build, installation and service commands
  -h, --help       show this help
USAGE
}

args=(--services)
dry_run=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root) args+=(--root "${2:?missing value for --root}"); shift 2 ;;
    --no-services) args+=(--no-services); shift ;;
    --dry-run) args+=(--dry-run); dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ "$dry_run" -eq 0 && "${AGL_NIX_VULKAN_ACTIVE:-0}" != 1 ]]; then
  exec "$script_dir/agl-nix-vulkan.sh" -- "$script_dir/install-agl-cargo.sh" "${args[@]}"
fi
exec "$script_dir/install-agl-cargo.sh" "${args[@]}"
