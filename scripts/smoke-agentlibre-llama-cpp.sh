#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
# shellcheck source=smoke-lib.sh
source "$script_dir/smoke-lib.sh"

config="${AGL_SMOKE_CONFIG:-}"
artifact_root="${AGL_SMOKE_ARTIFACT_ROOT:-/tmp/agl-016-llama-cpp-smoke}"
agl_bin="${AGL_SMOKE_AGL_BIN:-$repo_root/target/debug/agl}"
device="${AGL_SMOKE_DEVICE:-Vulkan0}"
run_suffix="agl-016-$(date +%s)-$$"
export AGL_HOME="${AGL_SMOKE_HOME:-${AGL_HOME:-$artifact_root/home-$run_suffix}}"

attempt_file() {
  local root="$1"
  local run_id="$2"
  local attempt_id="$3"
  local name="$4"
  printf '%s/runs/%s/attempts/%s/%s' "$root" "$run_id" "$attempt_id" "$name"
}

events_file() {
  local root="$1"
  local run_id="$2"
  printf '%s/runs/%s/events.jsonl' "$root" "$run_id"
}

need_tool cargo
need_tool grep
need_tool python3
need_tool readelf

[[ -n "$config" ]] || fail "AGL_SMOKE_CONFIG must point to a local inference TOML file"
[[ -f "$config" ]] || fail "missing smoke config: $config"
config="$(smoke_abs_path "$config")"

cd "$repo_root"
cargo build \
  -p agl-cli \
  -p agl-process \
  --bin agl \
  --bin agl-process-launcher
agl_bin="$(smoke_abs_path "$agl_bin")"

linked_libraries="$(readelf -d "$agl_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ "$linked_libraries" == *"libllama"* ]] || fail "$agl_bin is not linked to libllama"
[[ "$linked_libraries" == *"libggml"* ]] || fail "$agl_bin is not linked to libggml"

infer_root="$AGL_HOME/data"
inference_log="$AGL_HOME/state/logs/inference.log"
mkdir -p "$artifact_root"

"$agl_bin" inference run \
  --config "$config" \
  --max-output-tokens 32 \
  --prompt "Answer with exactly: agentLIBRE ok" \
  >"$artifact_root/infer-stdout.txt"

infer_run_id="$(single_run_id "$infer_root")"
infer_events="$(events_file "$infer_root" "$infer_run_id")"
infer_attempt_id="$(runtime_attempt_id "$infer_events" 1)"
infer_response="$(attempt_file "$infer_root" "$infer_run_id" "$infer_attempt_id" response.json)"
infer_runtime_log="$(attempt_file "$infer_root" "$infer_run_id" "$infer_attempt_id" runtime.log)"
infer_function_evidence="$infer_root/runs/$infer_run_id/function-resolution.json"
infer_content="$(json_content "$infer_response")"
[[ "$infer_content" == "agentLIBRE ok" ]] || fail "infer returned: $infer_content"
require_json_metadata_value "$infer_response" model_state loaded
require_json_metadata_value "$infer_response" selected_device "$device"
require_contains "$infer_events" '"backend":"llama_cpp"'
require_contains "$infer_runtime_log" "load_tensors: offloaded"
[[ ! -e "$infer_function_evidence" ]] || fail "raw inference run wrote function evidence"

echo "AGL_HOME: $AGL_HOME"
echo "config path: $config"
echo "infer artifact root: $infer_root"
echo "inference log: $inference_log"
echo "infer runtime log: $infer_runtime_log"
echo "linked llama.cpp libraries:"
echo "$linked_libraries"
echo "selected device: $device"
echo "AGL-016 llama.cpp smoke passed"
