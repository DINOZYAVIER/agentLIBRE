#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
# shellcheck source=smoke-lib.sh
source "$script_dir/smoke-lib.sh"

config="${AGL_SMOKE_CONFIG:-}"
artifact_root="${AGL_SMOKE_ARTIFACT_ROOT:-/tmp/agl-097-multiturn-smoke}"
agl_bin="${AGL_SMOKE_AGL_BIN:-$repo_root/target/debug/agl}"
max_output_tokens="${AGL_SMOKE_MAX_OUTPUT_TOKENS:-80}"
device="${AGL_SMOKE_DEVICE:-Vulkan0}"
run_suffix="agl-097-$(date +%s)-$$"
export AGL_HOME="${AGL_SMOKE_HOME:-${AGL_HOME:-$artifact_root/home-$run_suffix}}"

attempt_file() {
  local run_dir="$1"
  local attempt="$2"
  local name="$3"
  printf '%s/attempts/attempt-%04d/%s' "$run_dir" "$attempt" "$name"
}

chat_run_dir() {
  local session_id="$1"
  local run_id="$2"
  printf '%s/data/sessions/%s/runs/%s/inference-runs/%s' "$AGL_HOME" "$session_id" "$run_id" "$run_id"
}

session_transcript() {
  local session_id="$1"
  printf '%s/data/sessions/%s/transcript.jsonl' "$AGL_HOME" "$session_id"
}

require_response() {
  local path="$1"
  local content
  content="$(json_content "$path")"
  [[ -n "$content" ]] || fail "$path has empty assistant content"
  [[ "$content" != "<think>"* ]] || fail "$path starts with <think>"
  [[ "$content" != *$'\nUser:'* ]] || fail "$path contains generated User continuation"
  [[ "$content" != *$'\nAssistant:'* ]] || fail "$path contains generated Assistant continuation"
  [[ "$content" != *$'\nTool:'* ]] || fail "$path contains generated Tool continuation"
}

require_model_state() {
  local run_dir="$1"
  local attempt="$2"
  local state="$3"
  local runtime_log
  runtime_log="$(attempt_file "$run_dir" "$attempt" runtime.log)"
  require_contains "$runtime_log" "model_state = $state"
  require_contains "$runtime_log" "selected_device = $device"
  require_contains "$runtime_log" "load_tensors: offloaded"
}

require_jsonl_kind_count_at_least() {
  local path="$1"
  local kind="$2"
  local minimum="$3"
  local count
  count="$(python3 - "$path" "$kind" <<'PY'
import json
import sys

count = 0
with open(sys.argv[1], encoding="utf-8") as handle:
    for line in handle:
        if not line.strip():
            continue
        if json.loads(line).get("kind") == sys.argv[2]:
            count += 1
print(count)
PY
)"
  [[ "$count" -ge "$minimum" ]] || fail "$path has $count $kind events, expected at least $minimum"
}

run_chat_flow() {
  local name="$1"
  local session_id="$2"
  local run_id="$3"
  local input="$4"
  shift 4

  local stdout_path="$artifact_root/$name-stdout.txt"
  printf '%s' "$input" | "$agl_bin" chat \
    --config "$config" \
    --run-id "$run_id" \
    --session-id "$session_id" \
    --max-output-tokens "$max_output_tokens" \
    "$@" \
    >"$stdout_path"
  printf '%s' "$stdout_path"
}

need_tool cargo
need_tool grep
need_tool python3

[[ -n "$config" ]] || fail "AGL_SMOKE_CONFIG must point to a local inference TOML file"
[[ -f "$config" ]] || fail "missing smoke config: $config"
config="$(smoke_abs_path "$config")"

cd "$repo_root"
cargo build -p agl-cli
agl_bin="$(smoke_abs_path "$agl_bin")"
[[ -x "$agl_bin" ]] || fail "missing executable agl binary: $agl_bin"

mkdir -p "$artifact_root"

basic_session="$run_suffix-basic-session"
basic_run="$run_suffix-basic-run"
basic_stdout="$(run_chat_flow basic "$basic_session" "$basic_run" \
  $'Reply with one short sentence.\nReply with another short sentence.\n/exit\n')"
basic_dir="$(chat_run_dir "$basic_session" "$basic_run")"
basic_transcript="$(session_transcript "$basic_session")"
require_response "$(attempt_file "$basic_dir" 1 response.json)"
require_response "$(attempt_file "$basic_dir" 2 response.json)"
require_model_state "$basic_dir" 1 loaded
require_model_state "$basic_dir" 2 reused
require_jsonl_kind_count_at_least "$basic_transcript" "user_message" 2
require_jsonl_kind_count_at_least "$basic_transcript" "assistant_message" 2
require_contains "$basic_stdout" "session_id=$basic_session"

clear_session="$run_suffix-clear-session"
clear_run="$run_suffix-clear-run"
clear_stdout="$(run_chat_flow clear "$clear_session" "$clear_run" \
  $'Reply with one short sentence before clear.\n/clear\nReply with one short sentence after clear.\n/exit\n')"
clear_dir="$(chat_run_dir "$clear_session" "$clear_run")"
clear_transcript="$(session_transcript "$clear_session")"
require_response "$(attempt_file "$clear_dir" 1 response.json)"
require_response "$(attempt_file "$clear_dir" 2 response.json)"
require_model_state "$clear_dir" 1 loaded
require_model_state "$clear_dir" 2 loaded
require_contains "$clear_stdout" "context_cleared=true"
require_jsonl_kind_count_at_least "$clear_transcript" "context_cleared" 1

workspace_session="$run_suffix-workspace-session"
workspace_run="$run_suffix-workspace-run"
workspace="$artifact_root/workspace-$run_suffix"
mkdir -p "$workspace"
workspace_stdout="$(run_chat_flow workspace "$workspace_session" "$workspace_run" \
  "/workspace $workspace"$'\n/session\nReply with one short sentence about the workspace.\n/exit\n')"
workspace_dir="$(chat_run_dir "$workspace_session" "$workspace_run")"
require_response "$(attempt_file "$workspace_dir" 1 response.json)"
require_model_state "$workspace_dir" 1 loaded
require_contains "$workspace_stdout" "workspace_root=$workspace"
require_contains "$(attempt_file "$workspace_dir" 1 request.json)" "$workspace"

reload_session="$run_suffix-reload-session"
reload_run="$run_suffix-reload-run"
reload_stdout="$(run_chat_flow reload "$reload_session" "$reload_run" \
  $'/reload\nReply with one short sentence.\n/exit\n' \
  --function gemma4-12b)"
reload_dir="$(chat_run_dir "$reload_session" "$reload_run")"
require_response "$(attempt_file "$reload_dir" 1 response.json)"
require_model_state "$reload_dir" 1 loaded
require_contains "$reload_stdout" "context_reloaded=true"
require_contains "$reload_dir/function-resolution.json" '"id": "gemma4-12b"'
require_contains "$reload_dir/function-context.md" "<agentlibre_function_context>"
require_contains "$reload_dir/runtime-identity.json" '"gemma4-12b"'
require_contains "$(attempt_file "$reload_dir" 1 request.json)" "<agentlibre_function_context>"

echo "AGL_HOME: $AGL_HOME"
echo "config path: $config"
echo "artifact root: $artifact_root"
echo "basic stdout: $basic_stdout"
echo "clear stdout: $clear_stdout"
echo "workspace stdout: $workspace_stdout"
echo "reload stdout: $reload_stdout"
echo "basic run dir: $basic_dir"
echo "clear run dir: $clear_dir"
echo "workspace run dir: $workspace_dir"
echo "reload run dir: $reload_dir"
echo "AGL-097 multi-turn flow live smoke passed"
