#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
# shellcheck source=smoke-lib.sh
source "$script_dir/smoke-lib.sh"

config="${AGL_SMOKE_CONFIG:-}"
artifact_root="${AGL_SMOKE_ARTIFACT_ROOT:-/tmp/agl-056-skill-tools-smoke}"
agl_bin="${AGL_SMOKE_AGL_BIN:-$repo_root/target/debug/agl}"
max_output_tokens="${AGL_SMOKE_MAX_OUTPUT_TOKENS:-160}"
run_suffix="agl-056-$(date +%s)-$$"
export AGL_HOME="${AGL_SMOKE_HOME:-${AGL_HOME:-$artifact_root/home-$run_suffix}}"

run_id="$run_suffix-run"
run_root="$artifact_root/runs-$run_suffix"
workspace="$artifact_root/workspace-$run_suffix"
stdout_path="$artifact_root/stdout-$run_suffix.txt"

request_tool_context() {
  python3 - "$1" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    request = json.load(handle)

messages = request.get("messages")
if messages is None:
    messages = request.get("rendered", {}).get("messages", [])

for message in messages:
    content = message.get("content", "")
    if "<agentlibre_tool_context>" in content:
        print(content, end="")
        break
else:
    raise SystemExit("missing agentlibre_tool_context")
PY
}

latest_response_file() {
  local run_dir="$1"
  shopt -s nullglob
  local responses=("$run_dir"/attempts/attempt-*/response.json)
  shopt -u nullglob
  [[ ${#responses[@]} -gt 0 ]] || fail "no response artifacts under $run_dir"
  printf '%s\n' "${responses[$((${#responses[@]} - 1))]}"
}

need_tool cargo
need_tool git
need_tool grep
need_tool python3

[[ -n "$config" ]] || fail "AGL_SMOKE_CONFIG must point to a local inference TOML file"
[[ -f "$config" ]] || fail "missing smoke config: $config"
config="$(smoke_abs_path "$config")"

cd "$repo_root"
cargo build -p agl-cli
agl_bin="$(smoke_abs_path "$agl_bin")"
[[ -x "$agl_bin" ]] || fail "missing executable agl binary: $agl_bin"

mkdir -p "$workspace"
git -C "$workspace" init -q
cat >"$workspace/facts.txt" <<'EOF'
agentLIBRE skill tools smoke fixture
Expected final answer: skill tools smoke ok. Verification: fs.read loaded facts.txt.
EOF

prompt='You are testing agentLIBRE tool use. Your first model response must be only this exact tool call:
<tool_call>{"name":"fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>
After the tool observation, do not call another tool. Answer with exactly this single line:
skill tools smoke ok. Verification: fs.read loaded facts.txt.'

(
  cd "$workspace"
  "$agl_bin" inference run \
    --config "$config" \
    --artifact-root "$run_root" \
    --run-id "$run_id" \
    --workspace-root "$workspace" \
    --skill repo-status \
    --max-output-tokens "$max_output_tokens" \
    --prompt "$prompt" \
    >"$stdout_path"
)

run_dir="$run_root/inference-runs/$run_id"
events="$run_dir/agent-events.jsonl"
skill_context="$run_dir/skill-context.json"
request_1="$run_dir/attempts/attempt-0001/request.json"
response_1="$run_dir/attempts/attempt-0001/response.json"
runtime_1="$run_dir/attempts/attempt-0001/runtime.log"
runtime_2="$run_dir/attempts/attempt-0002/runtime.log"
tool_context="$artifact_root/tool-context-$run_suffix.txt"
request_tool_context "$request_1" >"$tool_context"
latest_response="$(latest_response_file "$run_dir")"
latest_content="$(json_content "$latest_response")"

require_contains "$skill_context" '"skill_id": "repo-status"'
require_contains "$skill_context" '"fs.read"'
require_contains "$skill_context" '"fs.list"'
require_contains "$skill_context" '"fs.search"'
require_contains "$skill_context" '"repo.status"'
require_not_contains "$skill_context" '"fs.edit"'
require_contains "$request_1" "<agentlibre_tool_context>"
require_contains "$request_1" "fs.read"
require_contains "$request_1" "fs.list"
require_contains "$request_1" "fs.search"
require_not_contains "$tool_context" "fs.edit"
require_contains "$response_1" "fs.read"
require_contains "$runtime_1" "model_state = loaded"
require_contains "$runtime_2" "model_state = reused"
require_not_contains "$runtime_2" "llama_cpp_session_reset_reason"
require_contains "$events" '"kind":"tool.call_started"'
require_contains "$events" '"kind":"tool.call_finished"'
require_contains "$events" '"name":"fs.read"'
require_contains "$events" '"kind":"turn.finished"'
require_contains "$events" '"status":"answered"'
require_contains "$stdout_path" "skill tools smoke ok"
require_contains "$stdout_path" "Verification:"
require_not_contains "$stdout_path" "stopped=true"
[[ ! -e "$run_dir/function-resolution.json" ]] || fail "raw inference run wrote function evidence"
[[ "$latest_content" == *"skill tools smoke ok"* ]] || fail "latest response did not contain expected final answer: $latest_content"
[[ "$latest_content" == *"Verification:"* ]] || fail "latest response did not contain verification evidence: $latest_content"

echo "AGL_HOME: $AGL_HOME"
echo "config path: $config"
echo "workspace root: $workspace"
echo "artifact root: $run_root"
echo "run dir: $run_dir"
echo "events: $events"
echo "skill context: $skill_context"
echo "tool context: $tool_context"
echo "first request: $request_1"
echo "first response: $response_1"
echo "first runtime log: $runtime_1"
echo "second runtime log: $runtime_2"
echo "latest response: $latest_response"
echo "AGL-056 skill tools live smoke passed"
