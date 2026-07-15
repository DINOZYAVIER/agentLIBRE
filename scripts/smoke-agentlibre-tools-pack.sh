#!/usr/bin/env bash
set -euo pipefail

# Opt-in local-model regression pack.
#
# Full run:
#   AGL_SMOKE_CONFIG=/absolute/or/relative/inference.toml \
#     scripts/smoke-agentlibre-tools-pack.sh
# The focused runtime cases require a GPU-offload config and default to
# AGL_SMOKE_DEVICE=Vulkan0; override that device when the config uses another GPU.
#
# Static validations only (no model or GGUF required):
#   AGL_SMOKE_STATIC_ONLY=1 scripts/smoke-agentlibre-tools-pack.sh
#
# Optional overrides:
#   AGL_SMOKE_AGL_BIN, AGL_SMOKE_ARTIFACT_ROOT,
#   AGL_SMOKE_MAX_OUTPUT_TOKENS, AGL_SMOKE_KEEP_WORKSPACES, AGL_SMOKE_DEVICE.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
# shellcheck source=smoke-lib.sh
source "$script_dir/smoke-lib.sh"

config="${AGL_SMOKE_CONFIG:-}"
artifact_root="${AGL_SMOKE_ARTIFACT_ROOT:-/tmp/agl-067-tools-pack}"
agl_bin="${AGL_SMOKE_AGL_BIN:-$repo_root/target/debug/agl}"
max_output_tokens="${AGL_SMOKE_MAX_OUTPUT_TOKENS:-192}"
static_only="${AGL_SMOKE_STATIC_ONLY:-0}"
keep_workspaces="${AGL_SMOKE_KEEP_WORKSPACES:-0}"
run_suffix="agl-067-$(date +%s)-$$"
tool_call_format=""

case "$static_only" in
  0 | 1) ;;
  *) fail "AGL_SMOKE_STATIC_ONLY must be 0 or 1" ;;
esac
case "$keep_workspaces" in
  0 | 1) ;;
  *) fail "AGL_SMOKE_KEEP_WORKSPACES must be 0 or 1" ;;
esac
[[ "$max_output_tokens" =~ ^[1-9][0-9]*$ ]] ||
  fail "AGL_SMOKE_MAX_OUTPUT_TOKENS must be a positive integer"

need_tool cargo
need_tool find
need_tool git
need_tool grep
need_tool ln
need_tool python3

if [[ "$static_only" == 0 ]]; then
  [[ -n "$config" ]] ||
    fail "AGL_SMOKE_CONFIG must point to a local inference TOML file"
  [[ -f "$config" ]] || fail "missing smoke config: $config"
  config="$(smoke_abs_path "$config")"
  tool_call_format="$(python3 - "$config" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as handle:
    config = tomllib.load(handle)
print(config.get("model", {}).get("tool_call_format", "hermes_json"))
PY
)"
fi

mkdir -p "$artifact_root"
artifact_root="$(cd -- "$artifact_root" && pwd)"
run_root="$artifact_root/$run_suffix"
workspaces_root="$run_root/workspaces"
evidence_root="$run_root/evidence"
focused_root="$run_root/focused"
summary_path="$run_root/cases.jsonl"
mkdir -p "$workspaces_root" "$evidence_root" "$focused_root"

pack_passed=0
cleanup() {
  if [[ "$pack_passed" == 1 && "$keep_workspaces" == 0 ]]; then
    rm -rf -- "$workspaces_root"
    rm -rf -- "$run_root/static/skills/workspace"
    find "$focused_root" -type d -name 'workspace-*' -prune -exec rm -rf -- {} +
  fi
}
trap cleanup EXIT

record_case() {
  local name="$1"
  local kind="$2"
  python3 - "$summary_path" "$name" "$kind" <<'PY'
import json
import sys

with open(sys.argv[1], "a", encoding="utf-8") as handle:
    json.dump({"case": sys.argv[2], "kind": sys.argv[3], "status": "passed"}, handle)
    handle.write("\n")
PY
  printf 'passed: %s\n' "$name"
}

record_skipped_case() {
  local name="$1"
  local reason="$2"
  python3 - "$summary_path" "$name" "$reason" <<'PY'
import json
import sys

with open(sys.argv[1], "a", encoding="utf-8") as handle:
    json.dump(
        {"case": sys.argv[2], "kind": "live", "status": "skipped", "reason": sys.argv[3]},
        handle,
    )
    handle.write("\n")
PY
  printf 'skipped: %s (%s)\n' "$name" "$reason"
}

tool_call_block() {
  local name="$1"
  local arguments="$2"
  python3 - "$tool_call_format" "$name" "$arguments" <<'PY'
import json
import sys

tool_format, name, raw_arguments = sys.argv[1:]
arguments = json.loads(raw_arguments)
if tool_format == "hermes_json":
    payload = {"name": name, "arguments": arguments}
    print(f"<tool_call>{json.dumps(payload, separators=(',', ':'))}</tool_call>")
elif tool_format == "gemma_function_call":
    def render(value):
        if isinstance(value, str):
            if '<|"|>' in value:
                raise SystemExit("Gemma smoke argument contains its string delimiter")
            return f'<|"|>{value}<|"|>'
        if isinstance(value, bool):
            return str(value).lower()
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return str(value)
        raise SystemExit(f"unsupported Gemma smoke argument: {value!r}")

    fields = ",".join(f"{key}:{render(value)}" for key, value in arguments.items())
    print(f"<|tool_call>call:{name}{{{fields}}}<tool_call|>")
else:
    raise SystemExit(f"unsupported tool_call_format: {tool_format}")
PY
}

require_event_sequence() {
  local path="$1"
  shift
  require_file "$path"
  python3 - "$path" "$@" <<'PY'
import json
import sys

path = sys.argv[1]
selectors = [json.loads(raw) for raw in sys.argv[2:]]
with open(path, encoding="utf-8") as handle:
    events = [json.loads(line) for line in handle if line.strip()]

cursor = 0
for selector in selectors:
    for index in range(cursor, len(events)):
        if all(events[index].get(key) == value for key, value in selector.items()):
            cursor = index + 1
            break
    else:
        raise SystemExit(
            f"{path}: missing ordered event {selector!r} after event index {cursor}"
        )
PY
}

require_no_event() {
  local path="$1"
  local selector="$2"
  require_file "$path"
  python3 - "$path" "$selector" <<'PY'
import json
import sys

selector = json.loads(sys.argv[2])
with open(sys.argv[1], encoding="utf-8") as handle:
    for line_number, line in enumerate(handle, 1):
        if not line.strip():
            continue
        event = json.loads(line)
        if all(event.get(key) == value for key, value in selector.items()):
            raise SystemExit(
                f"{sys.argv[1]}:{line_number}: unexpected event {selector!r}"
            )
PY
}

has_event() {
  local path="$1"
  local selector="$2"
  require_file "$path"
  python3 - "$path" "$selector" <<'PY'
import json
import sys

selector = json.loads(sys.argv[2])
with open(sys.argv[1], encoding="utf-8") as handle:
    events = [json.loads(line) for line in handle if line.strip()]
raise SystemExit(
    0
    if any(all(event.get(key) == value for key, value in selector.items()) for event in events)
    else 1
)
PY
}

require_event_count_at_least() {
  local path="$1"
  local selector="$2"
  local minimum="$3"
  require_file "$path"
  python3 - "$path" "$selector" "$minimum" <<'PY'
import json
import sys

selector = json.loads(sys.argv[2])
minimum = int(sys.argv[3])
with open(sys.argv[1], encoding="utf-8") as handle:
    events = [json.loads(line) for line in handle if line.strip()]
count = 0
for event in events:
    count += all(event.get(key) == value for key, value in selector.items())
if count < minimum:
    raise SystemExit(
        f"{sys.argv[1]}: found {count} events matching {selector!r}, expected {minimum}"
    )
PY
}

write_request_tool_context() {
  local request_path="$1"
  local output_path="$2"
  require_file "$request_path"
  python3 - "$request_path" "$output_path" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    request = json.load(handle)
messages = request.get("messages") or request.get("rendered", {}).get("messages", [])
for message in messages:
    content = message.get("content", "")
    if "<agentlibre_tool_context>" in content:
        with open(sys.argv[2], "w", encoding="utf-8") as output:
            output.write(content)
        break
else:
    raise SystemExit(f"{sys.argv[1]}: missing agentlibre_tool_context")
PY
}

require_jsonl_kinds() {
  local path="$1"
  shift
  require_file "$path"
  python3 - "$path" "$@" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    events = [json.loads(line) for line in handle if line.strip()]
for kind in sys.argv[2:]:
    if not any(event.get("kind") == kind for event in events):
        raise SystemExit(f"{sys.argv[1]}: missing transcript event kind {kind}")
PY
}

require_exact_event_values() {
  local path="$1"
  local kind="$2"
  local field="$3"
  shift 3
  require_file "$path"
  python3 - "$path" "$kind" "$field" "$@" <<'PY'
import json
import sys

path, kind, field, *expected = sys.argv[1:]
with open(path, encoding="utf-8") as handle:
    actual = [
        event.get(field)
        for line in handle
        if line.strip() and (event := json.loads(line)).get("kind") == kind
    ]
if actual != expected:
    raise SystemExit(f"{path}: {kind}.{field} values {actual!r}, expected {expected!r}")
PY
}

require_skill_context() {
  local path="$1"
  local skill_id="$2"
  local allowed_tool="$3"
  require_file "$path"
  python3 - "$path" "$skill_id" "$allowed_tool" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    skills = json.load(handle)
matches = [skill for skill in skills if skill.get("skill_id") == sys.argv[2]]
if len(matches) != 1:
    raise SystemExit(f"{sys.argv[1]}: expected one skill {sys.argv[2]!r}")
skill = matches[0]
if skill.get("source") != "core" or sys.argv[3] not in skill.get("allowed_tools", []):
    raise SystemExit(f"{sys.argv[1]}: incomplete core skill routing evidence: {skill}")
if not skill.get("required_hooks"):
    raise SystemExit(f"{sys.argv[1]}: selected skill has no required hook evidence")
PY
}

require_tree_not_contains() {
  local root="$1"
  local needle="$2"
  if grep -R -F -- "$needle" "$root" >/dev/null 2>&1; then
    fail "$root unexpectedly contains protected fixture content: $needle"
  fi
}

require_reused_attempts() {
  local run_dir="$1"
  shift
  local ordinal
  for ordinal in "$@"; do
    local attempt_id
    local response
    local runtime_log
    attempt_id="$(runtime_attempt_id "$run_dir/events.jsonl" "$ordinal")"
    printf -v response '%s/attempts/%s/response.json' "$run_dir" "$attempt_id"
    printf -v runtime_log '%s/attempts/%s/runtime.log' "$run_dir" "$attempt_id"
    require_json_metadata_value "$response" model_state reused
    require_not_contains "$runtime_log" "llama_cpp_session_reset_reason"
  done
  require_tree_not_contains "$run_dir" "rendered_history_not_appendable"
}

require_raw_inference_evidence() {
  local run_dir="$1"
  [[ ! -e "$run_dir/function-resolution.json" ]] ||
    fail "$run_dir unexpectedly contains function-resolution.json"
  [[ ! -e "$run_dir/function-context.md" ]] ||
    fail "$run_dir unexpectedly contains function-context.md"
}

start_live_case() {
  local name="$1"
  CASE_NAME="$name"
  CASE_ROOT="$run_root/live/$name"
  CASE_WORKSPACE="$workspaces_root/$name"
  CASE_HOME="$CASE_ROOT/home"
  CASE_ARTIFACTS="$CASE_ROOT/runs"
  CASE_STDOUT="$CASE_ROOT/stdout.txt"
  CASE_STDERR="$CASE_ROOT/stderr.txt"
  CASE_RUN_ID=""
  CASE_RUN_DIR=""
  CASE_EVENTS_RAW=""
  CASE_EVENTS=""
  CASE_STATUS=0
  mkdir -p "$CASE_ROOT" "$CASE_WORKSPACE" "$CASE_HOME" "$CASE_ARTIFACTS"
  git -C "$CASE_WORKSPACE" init -q
  printf 'running live case: %s\n' "$name"
  printf 'case root: %s\n' "$CASE_ROOT"
}

run_one_shot_case() {
  local tool_mode="$1"
  local skill="$2"
  local prompt="$3"

  set +e
  (
    cd "$CASE_WORKSPACE"
    AGL_HOME="$CASE_HOME" "$agl_bin" inference run \
      --config "$config" \
      --artifact-root "$CASE_ARTIFACTS" \
      --workspace-root "$CASE_WORKSPACE" \
      --max-output-tokens "$max_output_tokens" \
      --tool-mode "$tool_mode" \
      --skill "$skill" \
      --prompt "$prompt"
  ) >"$CASE_STDOUT" 2>"$CASE_STDERR"
  CASE_STATUS=$?
  set -e
  CASE_RUN_ID="$(single_run_id "$CASE_ARTIFACTS")"
  CASE_RUN_DIR="$CASE_ARTIFACTS/runs/$CASE_RUN_ID"
  CASE_EVENTS_RAW="$CASE_RUN_DIR/events.jsonl"
  CASE_EVENTS="$CASE_ROOT/events-normalized.jsonl"
  normalize_runtime_events "$CASE_EVENTS_RAW" "$CASE_EVENTS"
}

case_attempt_file() {
  local ordinal="$1"
  local name="$2"
  local attempt_id
  attempt_id="$(runtime_attempt_id "$CASE_EVENTS_RAW" "$ordinal")"
  printf '%s/attempts/%s/%s' "$CASE_RUN_DIR" "$attempt_id" "$name"
}

require_successful_case_process() {
  [[ "$CASE_STATUS" == 0 ]] || {
    sed -n '1,200p' "$CASE_STDERR" >&2
    fail "$CASE_NAME exited with status $CASE_STATUS; evidence: $CASE_ROOT"
  }
}

require_failed_case_process() {
  [[ "$CASE_STATUS" != 0 ]] ||
    fail "$CASE_NAME unexpectedly exited successfully; evidence: $CASE_ROOT"
}

run_static_skill_validations() {
  local case_root="$run_root/static/skills"
  local workspace="$case_root/workspace"
  local home="$case_root/home"
  local list_json="$case_root/core-list.json"
  local status_json="$case_root/workspace-status.json"
  local verify_json="$case_root/workspace-verify.json"
  local core_names="$case_root/core-names.txt"
  mkdir -p "$workspace" "$home"
  git -C "$workspace" init -q

  (
    cd "$workspace"
    AGL_HOME="$home" "$agl_bin" skill list --source core --json >"$list_json"
  )
  python3 - "$list_json" "$core_names" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    report = json.load(handle)
builtins = report.get("builtins", [])
if not builtins:
    raise SystemExit("core skill catalog is empty")
bad = [
    skill.get("name", "<unnamed>")
    for skill in builtins
    if skill.get("trust") != "TrustedByBinary" or skill.get("usable") is not True
]
if bad:
    raise SystemExit(f"core skills are not trusted and usable: {bad}")
required = {"repo-status", "skill"}
names = {skill["name"] for skill in builtins}
if names != required:
    raise SystemExit(f"core skills {sorted(names)!r}, expected {sorted(required)!r}")
for skill in builtins:
    if skill.get("source") != "core":
        raise SystemExit(f"builtin skill has non-core source: {skill}")
with open(sys.argv[2], "w", encoding="utf-8") as output:
    output.write("\n".join(sorted(names)) + "\n")
PY

  while IFS= read -r skill; do
    (
      cd "$workspace"
      AGL_HOME="$home" "$agl_bin" skill inspect "$skill" --runtime --json \
        >"$case_root/inspect-$skill.json"
    )
  done <"$core_names"
  python3 - "$case_root" "$core_names" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
names = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8").splitlines()
for name in names:
    report = json.loads((root / f"inspect-{name}.json").read_text(encoding="utf-8"))
    builtins = report.get("builtins", [])
    if report.get("name") != name or len(builtins) != 1 or report.get("workspace") != []:
        raise SystemExit(f"invalid runtime inspect report for {name}: {report}")
    skill = builtins[0]
    expected = {
        "name": name,
        "source": "core",
        "trust": "TrustedByBinary",
        "usable": True,
        "overridden_by_workspace": False,
    }
    if any(skill.get(key) != value for key, value in expected.items()):
        raise SystemExit(f"invalid runtime inspect identity for {name}: {skill}")
PY

  set +e
  (
    cd "$workspace"
    AGL_HOME="$home" "$agl_bin" skill status --json >"$status_json" \
      2>"$case_root/workspace-status.stderr"
  )
  local status_code=$?
  (
    cd "$workspace"
    AGL_HOME="$home" "$agl_bin" skill verify --json >"$verify_json" \
      2>"$case_root/workspace-verify.stderr"
  )
  local verify_code=$?
  set -e
  [[ "$status_code" != 0 ]] || fail "skill status accepted a workspace without a manifest"
  [[ "$verify_code" != 0 ]] || fail "skill verify accepted a workspace without a manifest"
  python3 - "$status_json" "$verify_json" <<'PY'
import json
import sys

for path in sys.argv[1:]:
    with open(path, encoding="utf-8") as handle:
        report = json.load(handle)
    if report.get("state") != "invalid":
        raise SystemExit(f"{path}: expected invalid workspace state")
    codes = {item.get("code") for item in report.get("diagnostics", [])}
    required = {"workspace_manifest_missing"}
    if not required <= codes:
        raise SystemExit(f"{path}: missing diagnostics {sorted(required - codes)}")
    if "skills_lock_missing" in codes:
        raise SystemExit(f"{path}: undeclared workspace skills unexpectedly require a lock")
PY
  record_case "builtin-skill-and-workspace-boundaries" "static"
}

run_static_repo_schema() {
  local case_root="$run_root/static/repo-schema"
  local tracked="$case_root/tracked-paths.txt"
  mkdir -p "$case_root"
  git -C "$repo_root" ls-files -- 'docs/specs/**' 'packs/**' '.agl/**' >"$tracked"
  python3 - "$tracked" <<'PY'
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    paths = [line.strip() for line in handle if line.strip()]
duplicates = [path for path in paths if path.startswith(("docs/specs/", "packs/"))]
if duplicates:
    raise SystemExit(f"tracked duplicate task/pack roots: {duplicates}")
agl_paths = {path for path in paths if path == ".agl" or path.startswith(".agl/")}
if agl_paths:
    raise SystemExit(f"tracked workspace-local .agl paths: {sorted(agl_paths)}")
PY
  record_case "repo-schema" "static"
}

run_focused_smoke() {
  local name="$1"
  local script="$2"
  local case_root="$focused_root/$name"
  local log="$case_root/smoke.log"
  mkdir -p "$case_root"
  printf 'running focused smoke: %s\n' "$name"
  printf 'focused case root: %s\n' "$case_root"
  if ! AGL_SMOKE_CONFIG="$config" \
    AGL_SMOKE_ARTIFACT_ROOT="$case_root/artifacts" \
    AGL_SMOKE_HOME="$case_root/home" \
    AGL_SMOKE_AGL_BIN="$agl_bin" \
    AGL_SMOKE_MAX_OUTPUT_TOKENS="$max_output_tokens" \
    "$script" >"$log" 2>&1; then
    sed -n '1,240p' "$log" >&2
    fail "focused smoke $name failed; full log: $log"
  fi
  record_case "$name" "focused-live"
}

run_read_list_search_case() {
  start_live_case "read-list-search"
  mkdir -p "$CASE_WORKSPACE/nested"
  printf '%s\n' 'AGL067_SEARCH_MARKER first fixture' >"$CASE_WORKSPACE/facts.txt"
  printf '%s\n' 'AGL067_SEARCH_MARKER nested fixture' >"$CASE_WORKSPACE/nested/more.txt"
  local list_call search_call prompt
  list_call="$(tool_call_block fs.list '{"path":".","recursive":true,"max_entries":20}')"
  search_call="$(tool_call_block fs.search '{"path":".","pattern":"AGL067_SEARCH_MARKER","max_matches":10}')"
  prompt="This is an agentLIBRE filesystem regression test. Your first response must be only this exact tool call:
$list_call
After that observation, your second response must be only this exact tool call:
$search_call
After the second observation, call no more tools and answer with exactly this single line: read list search complete. Verification: fs.list and fs.search completed."
  run_one_shot_case "read-only" "repo-status" "$prompt"
  require_successful_case_process

  require_event_sequence "$CASE_EVENTS" \
    '{"kind":"tool.call_started","name":"fs.list"}' \
    '{"kind":"tool.call_finished","name":"fs.list"}' \
    '{"kind":"observation.appended","name":"fs.list"}' \
    '{"kind":"tool.call_started","name":"fs.search"}' \
    '{"kind":"tool.call_finished","name":"fs.search"}' \
    '{"kind":"observation.appended","name":"fs.search"}' \
    '{"kind":"hook.batch_finished","event":"artifact.write","outcome":"pass"}' \
    '{"kind":"turn.finished","status":"answered"}'
  require_reused_attempts "$CASE_RUN_DIR" 2 3
  require_raw_inference_evidence "$CASE_RUN_DIR"
  require_exact_event_values "$CASE_EVENTS" "tool.call_started" "name" fs.list fs.search
  require_no_event "$CASE_EVENTS" '{"kind":"tool.call_started","name":"fs.edit"}'
  local tool_context="$CASE_ROOT/tool-context.txt"
  write_request_tool_context "$(case_attempt_file 1 request.json)" "$tool_context"
  require_contains "$tool_context" "fs.list"
  require_contains "$tool_context" "fs.search"
  require_not_contains "$tool_context" "fs.edit"
  python3 - "$(case_attempt_file 3 request.json)" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    request = json.load(handle)
messages = request.get("rendered", {}).get("messages", [])
tools = [message.get("content", "") for message in messages if message.get("role") == "tool"]
if len(tools) != 2:
    raise SystemExit(f"{sys.argv[1]}: expected two tool observations, got {len(tools)}")
expected_list = ["tool=fs.list", "path=.", "facts.txt", "nested/"]
expected_search = [
    "tool=fs.search",
    "path=.",
    "pattern=AGL067_SEARCH_MARKER",
    "matches=2",
    "facts.txt:1:AGL067_SEARCH_MARKER first fixture",
    "nested/more.txt:1:AGL067_SEARCH_MARKER nested fixture",
]
for needle in expected_list:
    if needle not in tools[0]:
        raise SystemExit(f"{sys.argv[1]}: list observation missing {needle!r}")
for needle in expected_search:
    if needle not in tools[1]:
        raise SystemExit(f"{sys.argv[1]}: search observation missing {needle!r}")
PY
  [[ "$(cat "$CASE_WORKSPACE/facts.txt")" == "AGL067_SEARCH_MARKER first fixture" ]] ||
    fail "$CASE_NAME mutated facts.txt in read-only mode"
  [[ "$(cat "$CASE_WORKSPACE/nested/more.txt")" == "AGL067_SEARCH_MARKER nested fixture" ]] ||
    fail "$CASE_NAME mutated nested/more.txt in read-only mode"
  record_case "$CASE_NAME" "live"
}

run_readonly_edit_rejection_case() {
  start_live_case "readonly-edit-rejection"
  printf '%s\n' 'alpha' >"$CASE_WORKSPACE/mutable.txt"
  local edit_call prompt
  edit_call="$(tool_call_block fs.edit '{"path":"mutable.txt","old_text":"alpha","new_text":"beta"}')"
  prompt="This is an agentLIBRE hidden-tool FSM regression test. Reproduce the requested action even though policy will reject it. Your response must be only this exact tool call:
$edit_call
Do not explain or choose another tool."
  run_one_shot_case "read-only" "skill" "$prompt"
  require_successful_case_process

  if has_event "$CASE_EVENTS" '{"kind":"tool.hidden_rejected","name":"fs.edit"}'; then
    require_event_sequence "$CASE_EVENTS" \
      '{"kind":"model.action_parsed"}' \
      '{"kind":"tool.hidden_rejected","name":"fs.edit"}' \
      '{"kind":"turn.stopped","reason":"hidden_tool"}' \
      '{"kind":"turn.finished","status":"stopped"}'
    printf '%s\n' 'runtime-hidden-tool-rejection' >"$evidence_root/readonly-edit-outcome.txt"
  else
    require_event_sequence "$CASE_EVENTS" \
      '{"kind":"model.action_parsed","action":"answer"}' \
      '{"kind":"answer.final"}' \
      '{"kind":"turn.finished","status":"answered"}'
    python3 - "$CASE_STDOUT" <<'PY'
import sys

content = open(sys.argv[1], encoding="utf-8").read().lower()
if "fs.edit" not in content:
    raise SystemExit(f"{sys.argv[1]}: model refusal did not identify fs.edit")
if not any(marker in content for marker in ("read-only", "not available", "unavailable", "cannot", "can't")):
    raise SystemExit(f"{sys.argv[1]}: model did not clearly refuse the hidden write tool")
PY
    printf '%s\n' 'model-policy-refusal' >"$evidence_root/readonly-edit-outcome.txt"
  fi
  require_raw_inference_evidence "$CASE_RUN_DIR"
  require_skill_context "$CASE_RUN_DIR/skill-context.json" "skill" "fs.edit"
  require_no_event "$CASE_EVENTS" '{"kind":"tool.call_started","name":"fs.edit"}'
  local tool_context="$CASE_ROOT/tool-context.txt"
  write_request_tool_context "$(case_attempt_file 1 request.json)" "$tool_context"
  require_not_contains "$tool_context" "fs.edit"
  [[ "$(cat "$CASE_WORKSPACE/mutable.txt")" == "alpha" ]] ||
    fail "$CASE_NAME mutated the rejected fixture"
  cp "$CASE_WORKSPACE/mutable.txt" "$evidence_root/readonly-edit-final.txt"
  record_case "$CASE_NAME" "live"
}

run_write_edit_case() {
  start_live_case "write-edit-success"
  printf '%s\n' 'alpha' >"$CASE_WORKSPACE/mutable.txt"
  local edit_call prompt
  edit_call="$(tool_call_block fs.edit '{"path":"mutable.txt","old_text":"alpha","new_text":"beta"}')"
  prompt="This is an agentLIBRE write-mode regression test. Your first response must be only this exact tool call:
$edit_call
After the tool observation, call no more tools and answer with exactly this single line: write edit complete. Verification: fs.edit changed mutable.txt from alpha to beta."
  run_one_shot_case "write" "skill" "$prompt"
  require_successful_case_process

  require_event_sequence "$CASE_EVENTS" \
    '{"kind":"tool.args_validated","name":"fs.edit"}' \
    '{"kind":"tool.call_started","name":"fs.edit"}' \
    '{"kind":"tool.call_finished","name":"fs.edit"}' \
    '{"kind":"observation.appended","name":"fs.edit"}' \
    '{"kind":"hook.batch_finished","event":"artifact.write","outcome":"pass"}' \
    '{"kind":"turn.finished","status":"answered"}'
  require_reused_attempts "$CASE_RUN_DIR" 2
  require_raw_inference_evidence "$CASE_RUN_DIR"
  require_skill_context "$CASE_RUN_DIR/skill-context.json" "skill" "fs.edit"
  require_exact_event_values "$CASE_EVENTS" "tool.call_started" "name" fs.edit
  require_no_event "$CASE_EVENTS" '{"kind":"tool.call_failed","name":"fs.edit"}'
  local tool_context="$CASE_ROOT/tool-context.txt"
  write_request_tool_context "$(case_attempt_file 1 request.json)" "$tool_context"
  require_contains "$tool_context" "fs.edit"
  [[ "$(cat "$CASE_WORKSPACE/mutable.txt")" == "beta" ]] ||
    fail "$CASE_NAME did not apply the exact edit"
  cp "$CASE_WORKSPACE/mutable.txt" "$evidence_root/write-edit-final.txt"
  record_case "$CASE_NAME" "live"
}

run_bad_path_case() {
  local name="$1"
  local tool="$2"
  local arguments="$3"
  local expected_path="$4"
  local expected_reason="$5"
  start_live_case "$name"
  local protected="AGL067_PROTECTED_${name}"
  printf '%s\n' "$protected" >"$CASE_ROOT/outside.txt"
  printf '%s\n' 'inside fixture' >"$CASE_WORKSPACE/inside.txt"
  if [[ "$name" == "bad-path-symlink" ]]; then
    ln -s "$CASE_ROOT/outside.txt" "$CASE_WORKSPACE/outside-link.txt"
  fi
  local call prompt
  call="$(tool_call_block "$tool" "$arguments")"
  prompt="This is an agentLIBRE path-guard regression test. Your response must be only this exact tool call:
$call
Do not explain or choose another path."
  run_one_shot_case "read-only" "repo-status" "$prompt"
  require_failed_case_process

  require_event_sequence "$CASE_EVENTS" \
    "{\"kind\":\"tool.args_validated\",\"name\":\"$tool\"}" \
    "{\"kind\":\"tool.call_started\",\"name\":\"$tool\"}" \
    "{\"kind\":\"tool.call_failed\",\"name\":\"$tool\"}" \
    '{"kind":"turn.finished","status":"failed"}'
  require_raw_inference_evidence "$CASE_RUN_DIR"
  require_exact_event_values "$CASE_EVENTS" "tool.call_started" "name" "$tool"
  require_no_event "$CASE_EVENTS" "{\"kind\":\"tool.call_finished\",\"name\":\"$tool\"}"
  local tool_context="$CASE_ROOT/tool-context.txt"
  write_request_tool_context "$(case_attempt_file 1 request.json)" "$tool_context"
  require_contains "$tool_context" "$tool"
  require_contains "$(case_attempt_file 1 response.json)" "$expected_path"
  require_contains "$CASE_STDERR" "$expected_reason"
  require_tree_not_contains "$CASE_ARTIFACTS" "$protected"
  require_not_contains "$CASE_STDOUT" "$protected"
  require_not_contains "$CASE_STDERR" "$protected"
  [[ "$(cat "$CASE_ROOT/outside.txt")" == "$protected" ]] ||
    fail "$CASE_NAME changed the protected fixture"
  record_case "$CASE_NAME" "live"
}

run_malformed_repair_case() {
  start_live_case "malformed-json-repair"
  printf '%s\n' 'AGL067_REPAIR_MARKER' >"$CASE_WORKSPACE/facts.txt"
  local malformed prompt
  malformed='<tool_call>"{\"name\":\"fs.read\",\"arguments\":{\"path\":\"facts.txt\",\"limit_lines\":20}}"</tool_call>'
  prompt="This is a parser conformance test. Your first response must be only this exact legacy sequence; do not correct or translate it:
$malformed
After the repaired tool observation, call no more tools and answer with exactly this single line: malformed repair complete. Verification: repaired fs.read completed."
  run_one_shot_case "read-only" "repo-status" "$prompt"
  require_successful_case_process

  require_event_sequence "$CASE_EVENTS" \
    '{"kind":"tool.json_malformed"}' \
    '{"kind":"tool.json_repair_attempted"}' \
    '{"kind":"tool.json_repair_succeeded"}' \
    '{"kind":"tool.call_started","name":"fs.read"}' \
    '{"kind":"tool.call_finished","name":"fs.read"}' \
    '{"kind":"hook.batch_finished","event":"artifact.write","outcome":"pass"}' \
    '{"kind":"turn.finished","status":"answered"}'
  require_reused_attempts "$CASE_RUN_DIR" 2
  require_raw_inference_evidence "$CASE_RUN_DIR"
  require_exact_event_values "$CASE_EVENTS" "tool.call_started" "name" fs.read
  require_contains "$(case_attempt_file 1 response.json)" "facts.txt"
  [[ "$(cat "$CASE_WORKSPACE/facts.txt")" == "AGL067_REPAIR_MARKER" ]] ||
    fail "$CASE_NAME mutated its read fixture"
  record_case "$CASE_NAME" "live"
}

run_malformed_rejection_case() {
  start_live_case "malformed-json-rejection"
  printf '%s\n' 'unchanged' >"$CASE_WORKSPACE/fixture.txt"
  local prompt
  if [[ "$tool_call_format" == "gemma_function_call" ]]; then
    prompt='This is a parser conformance test. Reproduce this exact intentionally incomplete native sequence as your entire response; do not correct it or add a terminator: <|tool_call>call:fs.read{path:<|"|>fixture.txt<|"|>}'
  else
    prompt='This is a parser conformance test. Reproduce this exact intentionally invalid legacy sequence as your entire response; do not correct or translate it: <tool_call>{"name":,"arguments":42</tool_call>'
  fi
  run_one_shot_case "read-only" "repo-status" "$prompt"
  require_successful_case_process

  require_event_sequence "$CASE_EVENTS" \
    '{"kind":"tool.json_malformed"}' \
    '{"kind":"tool.json_repair_attempted"}' \
    '{"kind":"tool.json_repair_failed"}' \
    '{"kind":"turn.stopped","reason":"tool_json_unrepairable"}' \
    '{"kind":"turn.finished","status":"stopped"}'
  require_raw_inference_evidence "$CASE_RUN_DIR"
  require_no_event "$CASE_EVENTS" '{"kind":"tool.call_started"}'
  [[ "$(cat "$CASE_WORKSPACE/fixture.txt")" == "unchanged" ]] ||
    fail "$CASE_NAME mutated its fixture"
  record_case "$CASE_NAME" "live"
}

run_tool_multiturn_case() {
  start_live_case "tool-multiturn-replay"
  printf '%s\n' 'AGL067_PRIOR_TOOL_OBSERVATION' >"$CASE_WORKSPACE/facts.txt"
  local session_id
  session_id="$(new_typed_id session)"
  local read_call first_prompt
  read_call="$(tool_call_block fs.read '{"path":"facts.txt","limit_lines":20}')"
  first_prompt="Your first response must be only this exact tool call: $read_call After the observation, call no more tools and answer with exactly this single line: first tool turn complete. Verification: fs.read completed."
  local second_prompt='Do not call a tool. Use the prior tool observation and answer with exactly this single line: AGL067_PRIOR_TOOL_OBSERVATION. Verification: prior fs.read observation retained.'

  set +e
  (
    cd "$CASE_WORKSPACE"
    printf '%s\n%s\n%s\n' "$first_prompt" "$second_prompt" '/exit' |
      AGL_HOME="$CASE_HOME" "$agl_bin" inference chat \
        --config "$config" \
        --artifact-root "$CASE_ARTIFACTS" \
        --session-id "$session_id" \
        --workspace-root "$CASE_WORKSPACE" \
        --max-output-tokens "$max_output_tokens" \
        --tool-mode read-only \
        --skill repo-status
  ) >"$CASE_STDOUT" 2>"$CASE_STDERR"
  CASE_STATUS=$?
  set -e
  require_successful_case_process

  local transcript="$CASE_HOME/data/sessions/$session_id/transcript.jsonl"
  local normalized_transcript="$CASE_ROOT/transcript-normalized.jsonl"
  normalize_transcript "$transcript" "$normalized_transcript"
  local -a turns
  mapfile -t turns < <(transcript_turn_ids "$normalized_transcript")
  [[ ${#turns[@]} -eq 2 ]] || fail "$CASE_NAME expected two turns, found ${#turns[@]}"
  local run_id_1 turn_id_1 run_id_2 turn_id_2
  IFS=$'\t' read -r run_id_1 turn_id_1 <<<"${turns[0]}"
  IFS=$'\t' read -r run_id_2 turn_id_2 <<<"${turns[1]}"
  [[ "$run_id_1" != "$run_id_2" ]] || fail "$CASE_NAME reused run ID $run_id_1"
  [[ "$turn_id_1" != "$turn_id_2" ]] || fail "$CASE_NAME reused turn ID $turn_id_1"
  local run_dir_1="$CASE_ARTIFACTS/runs/$run_id_1"
  local run_dir_2="$CASE_ARTIFACTS/runs/$run_id_2"
  local events_raw_1="$run_dir_1/events.jsonl"
  local events_raw_2="$run_dir_2/events.jsonl"
  local events_1="$CASE_ROOT/events-1-normalized.jsonl"
  local events_2="$CASE_ROOT/events-2-normalized.jsonl"
  normalize_runtime_events "$events_raw_1" "$events_1"
  normalize_runtime_events "$events_raw_2" "$events_2"
  local attempt_id_1 attempt_id_2 attempt_id_3
  attempt_id_1="$(runtime_attempt_id "$events_raw_1" 1)"
  attempt_id_2="$(runtime_attempt_id "$events_raw_1" 2)"
  attempt_id_3="$(runtime_attempt_id "$events_raw_2" 1)"
  local request_3="$run_dir_2/attempts/$attempt_id_3/request.json"
  local response_3="$run_dir_2/attempts/$attempt_id_3/response.json"
  local runtime_3="$run_dir_2/attempts/$attempt_id_3/runtime.log"
  require_event_sequence "$events_1" \
    '{"kind":"tool.call_started","name":"fs.read"}' \
    '{"kind":"tool.call_finished","name":"fs.read"}' \
    '{"kind":"observation.appended","name":"fs.read"}' \
    '{"kind":"hook.batch_finished","event":"artifact.write","outcome":"pass"}' \
    '{"kind":"turn.finished","status":"answered"}'
  require_event_sequence "$events_2" \
    '{"kind":"turn.started"}' \
    '{"kind":"hook.batch_finished","event":"artifact.write","outcome":"pass"}' \
    '{"kind":"turn.finished","status":"answered"}'
  require_event_count_at_least "$events_1" '{"kind":"turn.finished","status":"answered"}' 1
  require_event_count_at_least "$events_2" '{"kind":"turn.finished","status":"answered"}' 1
  require_reused_attempts "$run_dir_1" 2
  require_reused_attempts "$run_dir_2" 1
  require_raw_inference_evidence "$run_dir_1"
  require_raw_inference_evidence "$run_dir_2"
  require_exact_event_values "$events_1" "tool.call_started" "name" fs.read
  require_jsonl_kinds "$normalized_transcript" \
    user_message model_attempt_linked assistant_tool_call tool_message assistant_message
  require_file "$request_3"
  require_contains "$request_3" "AGL067_PRIOR_TOOL_OBSERVATION"
  require_contains "$normalized_transcript" '"kind":"assistant_tool_call"'
  require_contains "$normalized_transcript" '"kind":"tool_message"'
  require_contains "$normalized_transcript" '"name":"fs.read"'
  require_json_metadata_value "$response_3" model_state reused
  require_contains "$CASE_STDOUT" "assistant> AGL067_PRIOR_TOOL_OBSERVATION. Verification: prior fs.read observation retained."
  python3 - "$normalized_transcript" \
    "$run_id_1" "$turn_id_1" "$attempt_id_1" "$attempt_id_2" \
    "$run_id_2" "$turn_id_2" "$attempt_id_3" <<'PY'
import json
import sys
import uuid


def require_id(value, prefix):
    if not isinstance(value, str) or not value.startswith(f"{prefix}_"):
        raise SystemExit(f"expected {prefix}_ ID, got {value!r}")
    payload = value[len(prefix) + 1 :]
    try:
        parsed = uuid.UUID(payload)
    except ValueError as error:
        raise SystemExit(f"invalid {prefix}_ ID {value!r}") from error
    if str(parsed) != payload or parsed.version != 7:
        raise SystemExit(f"non-canonical UUIDv7 {prefix}_ ID {value!r}")

with open(sys.argv[1], encoding="utf-8") as handle:
    events = [json.loads(line) for line in handle if line.strip()]
run_id_1, turn_id_1, attempt_id_1, attempt_id_2 = sys.argv[2:6]
run_id_2, turn_id_2, attempt_id_3 = sys.argv[6:9]
kinds = [event.get("kind") for event in events]
expected = [
    "session_started",
    "user_message",
    "model_attempt_linked",
    "assistant_tool_call",
    "tool_message",
    "model_attempt_linked",
    "assistant_message",
    "user_message",
    "model_attempt_linked",
    "assistant_message",
    "session_finished",
]
if kinds != expected:
    raise SystemExit(f"{sys.argv[1]}: transcript kinds {kinds!r}, expected {expected!r}")
attempts = [event["attempt_id"] for event in events if event.get("kind") == "model_attempt_linked"]
if attempts != [attempt_id_1, attempt_id_2, attempt_id_3]:
    raise SystemExit(f"{sys.argv[1]}: attempt links {attempts!r}")
for event in events:
    if "message_id" in event:
        require_id(event["message_id"], "msg")
    if "attempt_id" in event:
        require_id(event["attempt_id"], "attempt")
correlated = [event for event in events if event.get("kind") not in {"session_started", "session_finished"}]
actual_pairs = [(event.get("run_id"), event.get("turn_id")) for event in correlated]
expected_pairs = [(run_id_1, turn_id_1)] * 6 + [(run_id_2, turn_id_2)] * 3
if actual_pairs != expected_pairs:
    raise SystemExit(
        f"{sys.argv[1]}: transcript correlations {actual_pairs!r}, expected {expected_pairs!r}"
    )
tool_call = next(event for event in events if event.get("kind") == "assistant_tool_call")
if tool_call.get("name") != "fs.read" or tool_call.get("arguments") != {
    "path": "facts.txt",
    "limit_lines": 20,
}:
    raise SystemExit(f"{sys.argv[1]}: unexpected tool call {tool_call!r}")
tool_message = next(event for event in events if event.get("kind") == "tool_message")
if tool_message.get("name") != "fs.read" or "AGL067_PRIOR_TOOL_OBSERVATION" not in tool_message.get("content", ""):
    raise SystemExit(f"{sys.argv[1]}: unexpected tool observation {tool_message!r}")
answers = [event.get("content", "") for event in events if event.get("kind") == "assistant_message"]
if len(answers) != 2 or "Verification:" not in answers[0] or answers[1] != "AGL067_PRIOR_TOOL_OBSERVATION. Verification: prior fs.read observation retained.":
    raise SystemExit(f"{sys.argv[1]}: unexpected answers {answers!r}")
PY
  [[ "$(cat "$CASE_WORKSPACE/facts.txt")" == "AGL067_PRIOR_TOOL_OBSERVATION" ]] ||
    fail "$CASE_NAME mutated its read fixture"
  record_case "$CASE_NAME" "live"
}

cd "$repo_root"
cargo build -p agl-cli
agl_bin="$(smoke_abs_path "$agl_bin")"
[[ -x "$agl_bin" ]] || fail "missing executable agl binary: $agl_bin"

run_static_skill_validations
run_static_repo_schema

if [[ "$static_only" == 1 ]]; then
  pack_passed=1
  echo "artifact root: $run_root"
  echo "case summary: $summary_path"
  echo "AGL-067 static tools/skills validations passed"
  exit 0
fi

run_focused_smoke "focused-skill-tools" "$script_dir/smoke-agentlibre-skill-tools.sh"
run_focused_smoke "focused-llama-cpp" "$script_dir/smoke-agentlibre-llama-cpp.sh"
run_focused_smoke "focused-multiturn-flows" "$script_dir/smoke-agentlibre-multiturn-flows.sh"

run_read_list_search_case
run_readonly_edit_rejection_case
run_write_edit_case
run_bad_path_case \
  "bad-path-parent" \
  "fs.list" \
  '{"path":"..","recursive":false,"max_entries":20}' \
  ".." \
  "parent traversal"
run_bad_path_case \
  "bad-path-absolute" \
  "fs.read" \
  "{\"path\":\"$run_root/live/bad-path-absolute/outside.txt\",\"limit_lines\":20}" \
  "$run_root/live/bad-path-absolute/outside.txt" \
  "cannot be absolute"
run_bad_path_case \
  "bad-path-symlink" \
  "fs.read" \
  '{"path":"outside-link.txt","limit_lines":20}' \
  "outside-link.txt" \
  "cannot traverse symlink"
if [[ "$tool_call_format" == "hermes_json" ]]; then
  run_malformed_repair_case
else
  record_skipped_case \
    "malformed-json-repair" \
    "tool format $tool_call_format has no safe malformed-call repair path"
fi
run_malformed_rejection_case
run_tool_multiturn_case

pack_passed=1
echo "config path: $config"
echo "tool call format: $tool_call_format"
echo "artifact root: $run_root"
echo "case summary: $summary_path"
echo "preserved fixture evidence: $evidence_root"
echo "AGL-067 tools and skills live regression pack passed"
