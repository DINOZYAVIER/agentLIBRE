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
  local attempt_id="$2"
  local name="$3"
  printf '%s/attempts/%s/%s' "$run_dir" "$attempt_id" "$name"
}

chat_run_dir() {
  local session_id="$1"
  local run_id="$2"
  printf '%s/data/sessions/%s/runs/%s' "$AGL_HOME" "$session_id" "$run_id"
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
  local attempt_id="$2"
  local state="$3"
  local response
  local runtime_log
  response="$(attempt_file "$run_dir" "$attempt_id" response.json)"
  runtime_log="$(attempt_file "$run_dir" "$attempt_id" runtime.log)"
  require_json_metadata_value "$response" model_state "$state"
  require_json_metadata_value "$response" selected_device "$device"
  if [[ "$state" == "loaded" ]]; then
    require_contains "$runtime_log" "load_tensors: offloaded"
  else
    require_not_contains "$runtime_log" "load_tensors: offloaded"
  fi
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
  local input="$3"
  shift 3

  local stdout_path="$artifact_root/$name-stdout.txt"
  printf '%s' "$input" | "$agl_bin" chat \
    --config "$config" \
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

models_path="$AGL_HOME/config/models.toml"
if [[ ! -f "$models_path" ]]; then
  mkdir -p "$(dirname -- "$models_path")"
  python3 - "$config" "$models_path" <<'PY'
import json
import pathlib
import sys
import tomllib

config_path, models_path = map(pathlib.Path, sys.argv[1:])
with config_path.open("rb") as handle:
    backend = tomllib.load(handle).get("backend", {})
model = backend.get("model")
projector = backend.get("multimodal_projector")
if not isinstance(model, str) or not model.strip():
    raise SystemExit(f"{config_path}: backend.model is required for the function smoke")
if not isinstance(projector, str) or not projector.strip():
    raise SystemExit(
        f"{config_path}: backend.multimodal_projector is required for gemma4-12b"
    )
models_path.write_text(
    "version = 1\n\n"
    "[models.gemma4-12b]\n"
    f"path = {json.dumps(model)}\n\n"
    "[models.gemma4-12b-mmproj]\n"
    f"path = {json.dumps(projector)}\n",
    encoding="utf-8",
)
PY
fi

cd "$repo_root"
cargo build -p agl-cli
agl_bin="$(smoke_abs_path "$agl_bin")"
[[ -x "$agl_bin" ]] || fail "missing executable agl binary: $agl_bin"

mkdir -p "$artifact_root"

basic_session="$(new_typed_id session)"
basic_stdout="$(run_chat_flow basic "$basic_session" \
  $'Reply with one short sentence.\nReply with another short sentence.\n/exit\n')"
basic_transcript="$(session_transcript "$basic_session")"
basic_normalized="$artifact_root/basic-transcript-normalized.jsonl"
normalize_transcript "$basic_transcript" "$basic_normalized"
mapfile -t basic_turns < <(transcript_turn_ids "$basic_normalized")
[[ ${#basic_turns[@]} -eq 2 ]] || fail "basic flow expected two turns, found ${#basic_turns[@]}"
IFS=$'\t' read -r basic_run_1 basic_turn_1 <<<"${basic_turns[0]}"
IFS=$'\t' read -r basic_run_2 basic_turn_2 <<<"${basic_turns[1]}"
[[ "$basic_run_1" != "$basic_run_2" ]] || fail "basic flow reused run ID $basic_run_1"
[[ "$basic_turn_1" != "$basic_turn_2" ]] || fail "basic flow reused turn ID $basic_turn_1"
basic_dir_1="$(chat_run_dir "$basic_session" "$basic_run_1")"
basic_dir_2="$(chat_run_dir "$basic_session" "$basic_run_2")"
basic_attempt_1="$(runtime_attempt_id "$basic_dir_1/events.jsonl" 1)"
basic_attempt_2="$(runtime_attempt_id "$basic_dir_2/events.jsonl" 1)"
require_response "$(attempt_file "$basic_dir_1" "$basic_attempt_1" response.json)"
require_response "$(attempt_file "$basic_dir_2" "$basic_attempt_2" response.json)"
require_model_state "$basic_dir_1" "$basic_attempt_1" loaded
require_model_state "$basic_dir_2" "$basic_attempt_2" reused
require_jsonl_kind_count_at_least "$basic_normalized" "user_message" 2
require_jsonl_kind_count_at_least "$basic_normalized" "assistant_message" 2
require_contains "$basic_stdout" "session_id=$basic_session"

clear_session="$(new_typed_id session)"
clear_stdout="$(run_chat_flow clear "$clear_session" \
  $'Reply with one short sentence before clear.\n/clear\nReply with one short sentence after clear.\n/exit\n')"
clear_transcript="$(session_transcript "$clear_session")"
clear_normalized="$artifact_root/clear-transcript-normalized.jsonl"
normalize_transcript "$clear_transcript" "$clear_normalized"
mapfile -t clear_turns < <(transcript_turn_ids "$clear_normalized")
[[ ${#clear_turns[@]} -eq 2 ]] || fail "clear flow expected two turns, found ${#clear_turns[@]}"
IFS=$'\t' read -r clear_run_1 clear_turn_1 <<<"${clear_turns[0]}"
IFS=$'\t' read -r clear_run_2 clear_turn_2 <<<"${clear_turns[1]}"
[[ "$clear_run_1" != "$clear_run_2" ]] || fail "clear flow reused run ID $clear_run_1"
[[ "$clear_turn_1" != "$clear_turn_2" ]] || fail "clear flow reused turn ID $clear_turn_1"
clear_dir_1="$(chat_run_dir "$clear_session" "$clear_run_1")"
clear_dir_2="$(chat_run_dir "$clear_session" "$clear_run_2")"
clear_attempt_1="$(runtime_attempt_id "$clear_dir_1/events.jsonl" 1)"
clear_attempt_2="$(runtime_attempt_id "$clear_dir_2/events.jsonl" 1)"
require_response "$(attempt_file "$clear_dir_1" "$clear_attempt_1" response.json)"
require_response "$(attempt_file "$clear_dir_2" "$clear_attempt_2" response.json)"
require_model_state "$clear_dir_1" "$clear_attempt_1" loaded
require_model_state "$clear_dir_2" "$clear_attempt_2" reused
require_contains "$(attempt_file "$clear_dir_2" "$clear_attempt_2" runtime.log)" \
  "cached_prompt_tokens = 0"
require_contains "$(attempt_file "$clear_dir_2" "$clear_attempt_2" runtime.log)" \
  "context_state = reused"
require_contains "$clear_stdout" "context_cleared=true"
require_jsonl_kind_count_at_least "$clear_normalized" "context_cleared" 1

workspace_session="$(new_typed_id session)"
workspace="$artifact_root/workspace-$run_suffix"
mkdir -p "$workspace"
workspace_stdout="$(run_chat_flow workspace "$workspace_session" \
  "/workspace $workspace"$'\n/session\nReply with one short sentence about the workspace.\n/exit\n')"
workspace_transcript="$(session_transcript "$workspace_session")"
workspace_normalized="$artifact_root/workspace-transcript-normalized.jsonl"
normalize_transcript "$workspace_transcript" "$workspace_normalized"
mapfile -t workspace_turns < <(transcript_turn_ids "$workspace_normalized")
[[ ${#workspace_turns[@]} -eq 1 ]] || fail "workspace flow expected one turn, found ${#workspace_turns[@]}"
IFS=$'\t' read -r workspace_run workspace_turn <<<"${workspace_turns[0]}"
workspace_dir="$(chat_run_dir "$workspace_session" "$workspace_run")"
workspace_attempt="$(runtime_attempt_id "$workspace_dir/events.jsonl" 1)"
require_response "$(attempt_file "$workspace_dir" "$workspace_attempt" response.json)"
require_model_state "$workspace_dir" "$workspace_attempt" loaded
require_contains "$workspace_stdout" "workspace_root=$workspace"
require_contains "$(attempt_file "$workspace_dir" "$workspace_attempt" request.json)" "$workspace"

reload_session="$(new_typed_id session)"
reload_stdout="$(run_chat_flow reload "$reload_session" \
  $'/reload\nReply with one short sentence.\n/exit\n' \
  --function gemma4-12b)"
reload_transcript="$(session_transcript "$reload_session")"
reload_normalized="$artifact_root/reload-transcript-normalized.jsonl"
normalize_transcript "$reload_transcript" "$reload_normalized"
mapfile -t reload_turns < <(transcript_turn_ids "$reload_normalized")
[[ ${#reload_turns[@]} -eq 1 ]] || fail "reload flow expected one turn, found ${#reload_turns[@]}"
IFS=$'\t' read -r reload_run reload_turn <<<"${reload_turns[0]}"
reload_dir="$(chat_run_dir "$reload_session" "$reload_run")"
reload_attempt="$(runtime_attempt_id "$reload_dir/events.jsonl" 1)"
require_response "$(attempt_file "$reload_dir" "$reload_attempt" response.json)"
require_model_state "$reload_dir" "$reload_attempt" loaded
require_contains "$reload_stdout" "context_reloaded=true"
require_contains "$reload_dir/function-resolution.json" '"id": "gemma4-12b"'
require_contains "$reload_dir/function-context.md" "<agentlibre_function_context>"
require_contains "$reload_dir/runtime-identity.json" '"gemma4-12b"'
require_contains "$(attempt_file "$reload_dir" "$reload_attempt" request.json)" "<agentlibre_function_context>"

echo "AGL_HOME: $AGL_HOME"
echo "config path: $config"
echo "artifact root: $artifact_root"
echo "basic stdout: $basic_stdout"
echo "clear stdout: $clear_stdout"
echo "workspace stdout: $workspace_stdout"
echo "reload stdout: $reload_stdout"
echo "basic run dirs: $basic_dir_1 $basic_dir_2"
echo "clear run dirs: $clear_dir_1 $clear_dir_2"
echo "workspace run dir: $workspace_dir"
echo "reload run dir: $reload_dir"
echo "AGL-097 multi-turn flow live smoke passed"
