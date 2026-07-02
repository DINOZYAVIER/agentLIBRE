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
  ci_fail "ubuntu-apt target accepts only --dry-run"
fi

ci_need_tool uname

packages=(
  binutils
  build-essential
  ca-certificates
  cmake
  curl
  git
  glslc
  glslang-tools
  libvulkan-dev
  pkg-config
  spirv-headers
  spirv-tools
)

if [[ "$dry_run" -eq 1 ]]; then
  ci_section "ubuntu-apt native dependency dry run"
  printf 'target=ubuntu-apt\n'
  printf 'package_manager=apt-get\n'
  printf 'packages=%s\n' "${packages[*]}"
  exit 0
fi

if ! command -v apt-get >/dev/null 2>&1; then
  ci_fail "ubuntu-apt target requires apt-get; install CMake, Vulkan, glslang, SPIR-V, and binutils manually or select another install-native target"
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  ci_fail "ubuntu-apt target supports apt-based Linux runners only"
fi

apt_prefix=()
if [[ "$(id -u)" -ne 0 ]]; then
  ci_need_tool sudo
  apt_prefix=(sudo)
fi

ci_section "Installing ubuntu-apt native dependencies"
ci_run "${apt_prefix[@]}" apt-get update
ci_run "${apt_prefix[@]}" apt-get install -y --no-install-recommends "${packages[@]}"
