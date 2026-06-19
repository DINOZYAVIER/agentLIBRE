#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool uname

if ! command -v apt-get >/dev/null 2>&1; then
  ci_fail "install-native-ubuntu.sh requires apt-get; install CMake, Vulkan, glslang, SPIR-V, and binutils manually on this runner"
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  ci_fail "install-native-ubuntu.sh supports Linux/Ubuntu runners only"
fi

apt_prefix=()
if [[ "$(id -u)" -ne 0 ]]; then
  ci_need_tool sudo
  apt_prefix=(sudo)
fi

ci_section "Installing Ubuntu native dependencies"
ci_run "${apt_prefix[@]}" apt-get update
ci_run "${apt_prefix[@]}" apt-get install -y --no-install-recommends \
  binutils \
  build-essential \
  ca-certificates \
  cmake \
  curl \
  git \
  glslc \
  glslang-tools \
  libvulkan-dev \
  pkg-config \
  spirv-headers \
  spirv-tools
