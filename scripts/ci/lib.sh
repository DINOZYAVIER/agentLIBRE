#!/usr/bin/env bash

if [[ -n "${AGL_CI_LIB_SOURCED:-}" ]]; then
  return 0
fi
AGL_CI_LIB_SOURCED=1

set -euo pipefail

agl_ci_script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
AGL_CI_REPO_ROOT="$(cd -- "$agl_ci_script_dir/../.." && pwd)"
export AGL_CI_REPO_ROOT

if [[ -d "$HOME/.cargo/bin" ]]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi

ci_section() {
  printf '\n==> %s\n' "$*"
}

ci_fail() {
  echo "ci failed: $*" >&2
  exit 1
}

ci_need_tool() {
  command -v "$1" >/dev/null 2>&1 || ci_fail "missing required tool: $1"
}

ci_run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

ci_cd_repo() {
  cd "$AGL_CI_REPO_ROOT"
}

ci_nproc() {
  if command -v nproc >/dev/null 2>&1; then
    nproc
  elif command -v sysctl >/dev/null 2>&1; then
    sysctl -n hw.ncpu
  else
    printf '2\n'
  fi
}

ci_default_jobs() {
  local cpu_count
  cpu_count="$(ci_nproc)"
  if [[ "$cpu_count" =~ ^[0-9]+$ && "$cpu_count" -gt 2 ]]; then
    printf '2\n'
  else
    printf '%s\n' "$cpu_count"
  fi
}

ci_ensure_submodule() {
  ci_need_tool git
  ci_cd_repo
  if [[ ! -f vendor/llama.cpp/CMakeLists.txt ]]; then
    ci_section "Initializing git submodules"
    ci_run git submodule update --init --recursive vendor/llama.cpp
  fi
  [[ -f vendor/llama.cpp/CMakeLists.txt ]] || ci_fail "missing vendor/llama.cpp; run git submodule update --init --recursive"
}
