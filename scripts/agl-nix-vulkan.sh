#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
nix_file="$repo_root/nix/agl-local-vulkan.nix"

usage() {
  cat <<'USAGE'
Usage:
  scripts/agl-nix-vulkan.sh
  scripts/agl-nix-vulkan.sh --diagnose
  scripts/agl-nix-vulkan.sh --build
  scripts/agl-nix-vulkan.sh --smoke-tools /path/to/local-inference.toml
  scripts/agl-nix-vulkan.sh --smoke-llama /path/to/local-inference.toml
  scripts/agl-nix-vulkan.sh --smoke-multiturn /path/to/local-inference.toml
  scripts/agl-nix-vulkan.sh -- <command> [args...]

Runs agentLIBRE local llama.cpp development commands inside a Nix shell with
Vulkan build and runtime variables set.
USAGE
}

quote_command() {
  local quoted=()
  local arg
  for arg in "$@"; do
    quoted+=("$(printf '%q' "$arg")")
  done
  printf '%s' "${quoted[*]}"
}

enter_nix_shell() {
  export AGL_NIX_VULKAN_ACTIVE=1
  local command
  if [[ $# -eq 0 ]]; then
    command="cd $(printf '%q' "$repo_root") && exec bash -l"
  else
    command="cd $(printf '%q' "$repo_root") && exec $(quote_command "$0" "$@")"
  fi
  exec nix-shell "$nix_file" --run "$command"
}

diagnose() {
  echo "repo: $repo_root"
  echo "AGL_NIX_VULKAN_SHELL=${AGL_NIX_VULKAN_SHELL:-}"
  echo "AGL_LLAMA_CPP_VULKAN_INCLUDE_DIR=${AGL_LLAMA_CPP_VULKAN_INCLUDE_DIR:-}"
  echo "AGL_LLAMA_CPP_VULKAN_LIBRARY=${AGL_LLAMA_CPP_VULKAN_LIBRARY:-}"
  echo "VK_DRIVER_FILES=${VK_DRIVER_FILES:-}"
  echo "VK_ICD_FILENAMES=${VK_ICD_FILENAMES:-}"
  echo "LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}"
  echo

  echo "vulkaninfo --summary:"
  vulkaninfo --summary
  echo

  if [[ -x "$repo_root/target/llama-cpp/build/bin/llama-completion" ]]; then
    echo "llama.cpp devices:"
    "$repo_root/target/llama-cpp/build/bin/llama-completion" --list-devices
    echo
  else
    echo "llama.cpp devices: target/llama-cpp/build/bin/llama-completion is not built"
    echo
  fi

  if [[ -x "$repo_root/target/debug/agl" ]]; then
    echo "target/debug/agl native links:"
    readelf -d "$repo_root/target/debug/agl" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true
  else
    echo "target/debug/agl native links: target/debug/agl is not built"
  fi
}

build_local() {
  "$repo_root/scripts/build-llama-cpp.sh"
  cargo build -p agl-cli
}

smoke_tools() {
  local config="${1:-}"
  [[ -n "$config" ]] || {
    echo "missing local inference config path" >&2
    exit 2
  }
  AGL_SMOKE_CONFIG="$config" "$repo_root/scripts/smoke-agentlibre-skill-tools.sh"
}

smoke_llama() {
  local config="${1:-}"
  [[ -n "$config" ]] || {
    echo "missing local inference config path" >&2
    exit 2
  }
  AGL_SMOKE_CONFIG="$config" "$repo_root/scripts/smoke-agentlibre-llama-cpp.sh"
}

smoke_multiturn() {
  local config="${1:-}"
  [[ -n "$config" ]] || {
    echo "missing local inference config path" >&2
    exit 2
  }
  AGL_SMOKE_CONFIG="$config" "$repo_root/scripts/smoke-agentlibre-multiturn-flows.sh"
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ "${AGL_NIX_VULKAN_ACTIVE:-}" != "1" ]]; then
  enter_nix_shell "$@"
fi

cd "$repo_root"

case "${1:-}" in
  "")
    exec bash -l
    ;;
  -h|--help)
    usage
    ;;
  --diagnose)
    diagnose
    ;;
  --build)
    build_local
    ;;
  --smoke-tools)
    shift
    smoke_tools "$@"
    ;;
  --smoke-llama)
    shift
    smoke_llama "$@"
    ;;
  --smoke-multiturn)
    shift
    smoke_multiturn "$@"
    ;;
  --)
    shift
    exec "$@"
    ;;
  *)
    exec "$@"
    ;;
esac
