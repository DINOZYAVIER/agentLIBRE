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
  ci_fail "fedora-dnf target accepts only --dry-run"
fi

packages=(
  binutils
  ca-certificates
  cmake
  curl
  gcc
  gcc-c++
  git
  glslang
  glslc
  make
  pkgconf-pkg-config
  spirv-headers
  spirv-tools
  vulkan-headers
)

if [[ "$dry_run" -eq 1 ]]; then
  ci_section "fedora-dnf native dependency dry run"
  printf 'target=fedora-dnf\n'
  printf 'package_manager=dnf\n'
  printf 'packages=%s\n' "${packages[*]}"
  exit 0
fi

command -v dnf >/dev/null 2>&1 || ci_fail "fedora-dnf target requires dnf"
if [[ "$(id -u)" -ne 0 ]]; then
  ci_need_tool sudo
  prefix=(sudo)
else
  prefix=()
fi

ci_section "Installing fedora-dnf native dependencies"
ci_run "${prefix[@]}" dnf install -y "${packages[@]}"
