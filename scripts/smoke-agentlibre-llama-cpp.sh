#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

model="${AGL_SMOKE_MODEL_GGUF:-${AGL_TEST_MODEL_GGUF:-}}"
engine="${AGL_LLAMA_SERVER:-$repo_root/target/llama-cpp/build/bin/llama-server}"

[[ -n "$model" && -f "$model" ]] || {
  echo "AGL_SMOKE_MODEL_GGUF must name an installed GGUF file" >&2
  exit 2
}
if [[ ! -x "$engine" ]]; then
  "$repo_root/scripts/build-llama-cpp.sh"
fi

cd "$repo_root"
AGL_LLAMA_SERVER="$engine" AGL_TEST_MODEL_GGUF="$model" \
  cargo test -p agl-inference --test agl173_live_server -- --ignored --nocapture

echo "AGL-173 private llama-server smoke passed"
