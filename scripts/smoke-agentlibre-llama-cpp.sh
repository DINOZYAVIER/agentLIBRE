#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

config="${AGL_SMOKE_CONFIG:-}"
artifact_root="${AGL_SMOKE_ARTIFACT_ROOT:-/tmp/agl-015-llama-cpp-smoke}"
agentlibre_bin="${AGL_SMOKE_AGENTLIBRE_BIN:-$repo_root/target/debug/agentLIBRE}"
device="${AGL_SMOKE_DEVICE:-Vulkan0}"
run_suffix="agl-015-$$"

fail() {
  echo "smoke failed: $*" >&2
  exit 1
}

need_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_contains() {
  local path="$1"
  local needle="$2"
  require_file "$path"
  grep -F -- "$needle" "$path" >/dev/null || fail "$path does not contain: $needle"
}

json_content() {
  python3 - "$1" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    print(json.load(handle)["content"], end="")
PY
}

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

linked_libraries="$(readelf -d "$agentlibre_bin" | grep -E 'NEEDED.*(libllama|libggml)|RUNPATH' || true)"
[[ "$linked_libraries" == *"libllama"* ]] || fail "$agentlibre_bin is not linked to libllama"
[[ "$linked_libraries" == *"libggml"* ]] || fail "$agentlibre_bin is not linked to libggml"

infer_run_id="$run_suffix-infer"
chat_run_id="$run_suffix-chat"
infer_root="$artifact_root/infer-$run_suffix"
chat_root="$artifact_root/chat-$run_suffix"
mkdir -p "$infer_root" "$chat_root"

"$agentlibre_bin" infer \
  --config "$config" \
  --artifact-root "$infer_root" \
  --run-id "$infer_run_id" \
  --max-output-tokens 32 \
  --prompt "Answer with exactly: agentLIBRE ok" \
  >"$infer_root/stdout.txt"

infer_response="$(attempt_file "$infer_root" "$infer_run_id" 1 response.json)"
infer_stderr="$(attempt_file "$infer_root" "$infer_run_id" 1 stderr.log)"
infer_events="$(events_file "$infer_root" "$infer_run_id")"
infer_content="$(json_content "$infer_response")"
[[ "$infer_content" == "agentLIBRE ok" ]] || fail "infer returned: $infer_content"
require_contains "$infer_events" '"backend":"llama_cpp"'
require_contains "$infer_stderr" "selected_device = $device"
require_contains "$infer_stderr" "load_tensors: offloaded"

printf '%s\n%s\n%s\n' \
  "Reply with one short sentence." \
  "Reply with another short sentence." \
  "/exit" \
  | "$agentlibre_bin" chat \
      --config "$config" \
      --artifact-root "$chat_root" \
      --run-id "$chat_run_id" \
      --max-output-tokens 48 \
      >"$chat_root/stdout.txt"

chat_response_1="$(attempt_file "$chat_root" "$chat_run_id" 1 response.json)"
chat_response_2="$(attempt_file "$chat_root" "$chat_run_id" 2 response.json)"
chat_stderr_1="$(attempt_file "$chat_root" "$chat_run_id" 1 stderr.log)"
chat_stderr_2="$(attempt_file "$chat_root" "$chat_run_id" 2 stderr.log)"
chat_events="$(events_file "$chat_root" "$chat_run_id")"

reject_generated_continuation "$chat_response_1"
reject_generated_continuation "$chat_response_2"
require_contains "$chat_events" '"backend":"llama_cpp"'
require_contains "$chat_stderr_1" "model_state = loaded"
require_contains "$chat_stderr_1" "selected_device = $device"
require_contains "$chat_stderr_1" "load_tensors: offloaded"
require_contains "$chat_stderr_2" "model_state = reused"
require_contains "$chat_stderr_2" "selected_device = $device"
require_contains "$chat_stderr_2" "load_tensors: offloaded"

echo "infer artifact root: $infer_root"
echo "chat artifact root: $chat_root"
echo "linked llama.cpp libraries:"
echo "$linked_libraries"
echo "selected device: $device"
echo "chat attempt 1 model state: $(model_state "$chat_stderr_1")"
echo "chat attempt 2 model state: $(model_state "$chat_stderr_2")"
echo "AGL-015 llama.cpp smoke passed"
