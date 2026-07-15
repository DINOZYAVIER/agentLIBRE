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
cargo build -p agl-cli
agl_bin="$(smoke_abs_path "$agl_bin")"

linked_libraries="$(readelf -d "$agl_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ "$linked_libraries" == *"libllama"* ]] || fail "$agl_bin is not linked to libllama"
[[ "$linked_libraries" == *"libggml"* ]] || fail "$agl_bin is not linked to libggml"

chat_session_id="$(new_typed_id session)"
infer_root="$AGL_HOME/data"
chat_root="$AGL_HOME/data/sessions/$chat_session_id"
app_log="$AGL_HOME/state/logs/agentLIBRE.log"
inference_log="$AGL_HOME/state/logs/inference.log"
session_transcript="$AGL_HOME/data/sessions/$chat_session_id/transcript.jsonl"
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

printf '%s\n%s\n%s\n' \
  "Reply with one short sentence." \
  "Reply with another short sentence." \
  "/exit" \
  | "$agl_bin" inference chat \
      --config "$config" \
      --session-id "$chat_session_id" \
      --max-output-tokens 48 \
      >"$artifact_root/chat-stdout.txt"

normalized_transcript="$artifact_root/chat-transcript-normalized.jsonl"
normalize_transcript "$session_transcript" "$normalized_transcript"
mapfile -t chat_turns < <(transcript_turn_ids "$normalized_transcript")
[[ ${#chat_turns[@]} -eq 2 ]] || fail "expected two transcript turns, found ${#chat_turns[@]}"
IFS=$'\t' read -r chat_run_id_1 chat_turn_id_1 <<<"${chat_turns[0]}"
IFS=$'\t' read -r chat_run_id_2 chat_turn_id_2 <<<"${chat_turns[1]}"
[[ "$chat_run_id_1" != "$chat_run_id_2" ]] || fail "chat turns reused run ID $chat_run_id_1"
[[ "$chat_turn_id_1" != "$chat_turn_id_2" ]] || fail "chat turns reused turn ID $chat_turn_id_1"

chat_events_1="$(events_file "$chat_root" "$chat_run_id_1")"
chat_events_2="$(events_file "$chat_root" "$chat_run_id_2")"
chat_attempt_id_1="$(runtime_attempt_id "$chat_events_1" 1)"
chat_attempt_id_2="$(runtime_attempt_id "$chat_events_2" 1)"
chat_response_1="$(attempt_file "$chat_root" "$chat_run_id_1" "$chat_attempt_id_1" response.json)"
chat_response_2="$(attempt_file "$chat_root" "$chat_run_id_2" "$chat_attempt_id_2" response.json)"
chat_runtime_log_1="$(attempt_file "$chat_root" "$chat_run_id_1" "$chat_attempt_id_1" runtime.log)"
chat_runtime_log_2="$(attempt_file "$chat_root" "$chat_run_id_2" "$chat_attempt_id_2" runtime.log)"

reject_generated_continuation "$chat_response_1"
reject_generated_continuation "$chat_response_2"
require_contains "$chat_events_1" '"backend":"llama_cpp"'
require_contains "$chat_events_2" '"backend":"llama_cpp"'
require_json_metadata_value "$chat_response_1" model_state loaded
require_json_metadata_value "$chat_response_1" selected_device "$device"
require_contains "$chat_runtime_log_1" "load_tensors: offloaded"
require_json_metadata_value "$chat_response_2" model_state reused
require_json_metadata_value "$chat_response_2" selected_device "$device"
require_contains "$chat_runtime_log_2" "context_state = reused"
require_not_contains "$chat_runtime_log_2" "load_tensors: offloaded"
require_contains "$normalized_transcript" '"kind":"user_message"'
require_contains "$normalized_transcript" '"kind":"assistant_message"'
require_contains "$app_log" "chat session started"
[[ ! -e "$chat_root/runs/$chat_run_id_1/function-resolution.json" ]] ||
  fail "raw inference chat wrote function evidence for $chat_run_id_1"
[[ ! -e "$chat_root/runs/$chat_run_id_2/function-resolution.json" ]] ||
  fail "raw inference chat wrote function evidence for $chat_run_id_2"

echo "AGL_HOME: $AGL_HOME"
echo "config path: $config"
echo "infer artifact root: $infer_root"
echo "chat artifact root: $chat_root"
echo "chat turn 1: $chat_run_id_1/$chat_turn_id_1"
echo "chat turn 2: $chat_run_id_2/$chat_turn_id_2"
echo "session transcript: $session_transcript"
echo "app log: $app_log"
echo "inference log: $inference_log"
echo "infer runtime log: $infer_runtime_log"
echo "chat attempt 1 runtime log: $chat_runtime_log_1"
echo "chat attempt 2 runtime log: $chat_runtime_log_2"
echo "linked llama.cpp libraries:"
echo "$linked_libraries"
echo "selected device: $device"
echo "chat attempt 1 model state: $(json_metadata_value "$chat_response_1" model_state)"
echo "chat attempt 2 model state: $(json_metadata_value "$chat_response_2" model_state)"
echo "AGL-016 llama.cpp smoke passed"
