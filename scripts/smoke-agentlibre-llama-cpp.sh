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

reject_generated_continuation() {
  local path="$1"
  local content
  content="$(json_content "$path")"
  [[ -n "$content" ]] || fail "$path has empty assistant content"
  [[ "$content" != "<think>"* ]] || fail "$path starts with <think>"
  [[ "$content" != *$'\nUser:'* ]] || fail "$path contains generated User continuation"
  [[ "$content" != *$'\nAssistant:'* ]] || fail "$path contains generated Assistant continuation"
  [[ "$content" != *$'\nTool:'* ]] || fail "$path contains generated Tool continuation"
}

attempt_file() {
  local root="$1"
  local run_id="$2"
  local attempt="$3"
  local name="$4"
  printf '%s/inference-runs/%s/attempts/attempt-%04d/%s' "$root" "$run_id" "$attempt" "$name"
}

events_file() {
  local root="$1"
  local run_id="$2"
  printf '%s/inference-runs/%s/events.jsonl' "$root" "$run_id"
}

model_state() {
  local path="$1"
  grep -F -m 1 "model_state = " "$path" | sed 's/^model_state = //'
}

need_tool cargo
need_tool grep
need_tool python3
need_tool readelf

[[ -n "$config" ]] || fail "AGL_SMOKE_CONFIG must point to a local inference TOML file"
[[ -f "$config" ]] || fail "missing smoke config: $config"

cd "$repo_root"
cargo build -p agl-cli

linked_libraries="$(readelf -d "$agl_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ "$linked_libraries" == *"libllama"* ]] || fail "$agl_bin is not linked to libllama"
[[ "$linked_libraries" == *"libggml"* ]] || fail "$agl_bin is not linked to libggml"

infer_run_id="$run_suffix-infer"
chat_run_id="$run_suffix-chat"
chat_session_id="$run_suffix-session"
infer_root="$AGL_HOME/data/runs"
chat_root="$AGL_HOME/data/sessions/$chat_session_id/runs/$chat_run_id"
app_log="$AGL_HOME/state/logs/agentLIBRE.log"
inference_log="$AGL_HOME/state/logs/inference.log"
session_transcript="$AGL_HOME/data/sessions/$chat_session_id/transcript.jsonl"
mkdir -p "$artifact_root"

"$agl_bin" run \
  --config "$config" \
  --run-id "$infer_run_id" \
  --max-output-tokens 32 \
  --prompt "Answer with exactly: agentLIBRE ok" \
  >"$artifact_root/infer-stdout.txt"

infer_response="$(attempt_file "$infer_root" "$infer_run_id" 1 response.json)"
infer_runtime_log="$(attempt_file "$infer_root" "$infer_run_id" 1 runtime.log)"
infer_events="$(events_file "$infer_root" "$infer_run_id")"
infer_content="$(json_content "$infer_response")"
[[ "$infer_content" == "agentLIBRE ok" ]] || fail "infer returned: $infer_content"
require_contains "$infer_events" '"backend":"llama_cpp"'
require_contains "$infer_runtime_log" "selected_device = $device"
require_contains "$infer_runtime_log" "load_tensors: offloaded"

printf '%s\n%s\n%s\n' \
  "Reply with one short sentence." \
  "Reply with another short sentence." \
  "/exit" \
  | "$agl_bin" chat \
      --config "$config" \
      --run-id "$chat_run_id" \
      --session-id "$chat_session_id" \
      --max-output-tokens 48 \
      >"$artifact_root/chat-stdout.txt"

chat_response_1="$(attempt_file "$chat_root" "$chat_run_id" 1 response.json)"
chat_response_2="$(attempt_file "$chat_root" "$chat_run_id" 2 response.json)"
chat_runtime_log_1="$(attempt_file "$chat_root" "$chat_run_id" 1 runtime.log)"
chat_runtime_log_2="$(attempt_file "$chat_root" "$chat_run_id" 2 runtime.log)"
chat_events="$(events_file "$chat_root" "$chat_run_id")"

reject_generated_continuation "$chat_response_1"
reject_generated_continuation "$chat_response_2"
require_contains "$chat_events" '"backend":"llama_cpp"'
require_contains "$chat_runtime_log_1" "model_state = loaded"
require_contains "$chat_runtime_log_1" "selected_device = $device"
require_contains "$chat_runtime_log_1" "load_tensors: offloaded"
require_contains "$chat_runtime_log_2" "model_state = reused"
require_contains "$chat_runtime_log_2" "selected_device = $device"
require_contains "$chat_runtime_log_2" "load_tensors: offloaded"
require_contains "$session_transcript" '"kind":"user_message"'
require_contains "$session_transcript" '"kind":"assistant_message"'
require_contains "$app_log" "chat session started"
require_contains "$inference_log" "llama.cpp inference attempt succeeded"

echo "AGL_HOME: $AGL_HOME"
echo "config path: $config"
echo "infer artifact root: $infer_root"
echo "chat artifact root: $chat_root"
echo "session transcript: $session_transcript"
echo "app log: $app_log"
echo "inference log: $inference_log"
echo "infer runtime log: $infer_runtime_log"
echo "chat attempt 1 runtime log: $chat_runtime_log_1"
echo "chat attempt 2 runtime log: $chat_runtime_log_2"
echo "linked llama.cpp libraries:"
echo "$linked_libraries"
echo "selected device: $device"
echo "chat attempt 1 model state: $(model_state "$chat_runtime_log_1")"
echo "chat attempt 2 model state: $(model_state "$chat_runtime_log_2")"
echo "AGL-016 llama.cpp smoke passed"
