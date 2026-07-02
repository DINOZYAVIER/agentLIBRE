#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
source "$script_dir/../lib.sh"

dry_run=0
if [[ "${1:-}" == "--dry-run" ]]; then
  dry_run=1
  shift
fi
if [[ "$#" -gt 0 ]]; then
  ci_fail "arch-pacman target accepts only --dry-run"
fi

packages=(
  base-devel
  binutils
  cmake
  curl
  git
  glslang
  pkgconf
  shaderc
  spirv-headers
  spirv-tools
  vulkan-headers
)

if [[ "$dry_run" -eq 1 ]]; then
  ci_section "arch-pacman native dependency dry run"
  printf 'target=arch-pacman\n'
  printf 'package_manager=pacman\n'
  printf 'packages=%s\n' "${packages[*]}"
  exit 0
fi

command -v pacman >/dev/null 2>&1 || ci_fail "arch-pacman target requires pacman"
if [[ "$(id -u)" -ne 0 ]]; then
  ci_need_tool sudo
  prefix=(sudo)
else
  prefix=()
fi

ci_section "Installing arch-pacman native dependencies"
ci_run "${prefix[@]}" pacman -Syu --needed --noconfirm "${packages[@]}"
