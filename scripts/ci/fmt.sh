#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool cargo
ci_need_tool clang-format

ci_cd_repo

ci_section "Rust format"
ci_run cargo fmt --all -- --check

ci_section "llama.cpp bridge format"
ci_run clang-format \
  --style=file:vendor/llama.cpp/.clang-format \
  --dry-run \
  --Werror \
  crates/agl-llama-cpp-sys/src/native/abi_guard.cpp \
  crates/agl-llama-cpp-sys/src/native/chat_template_bridge.cpp \
  crates/agl-llama-cpp-sys/src/native/mtmd_bridge.cpp \
  crates/agl-llama-cpp-sys/src/native/mtp_speculative_bridge.cpp
