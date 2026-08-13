#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"
cd "$repo_root"

: "${AGL_TEST_MODEL_GGUF:?AGL_TEST_MODEL_GGUF must name the installed 32K GGUF model}"
[[ -f "$AGL_TEST_MODEL_GGUF" ]] || {
  printf 'AGL-178 live model does not exist: %s\n' "$AGL_TEST_MODEL_GGUF" >&2
  exit 2
}
command -v systemctl >/dev/null 2>&1 || {
  printf 'AGL-178 live acceptance requires systemctl --user\n' >&2
  exit 2
}

cargo test -p agl-runtime --test agl178_installed_live -- \
  --ignored --exact fresh_installed_product_runs_terminal_effect_and_32k_vulkan_inference \
  --nocapture
