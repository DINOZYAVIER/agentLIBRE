#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
artifact_root="${AGL_SMOKE_ARTIFACT_ROOT:-/tmp/agl-function-live-smoke}"
run_suffix="$(date +%s)-$$"
agl_bin="${AGL_SMOKE_AGL_BIN:-${AGL_BIN:-$repo_root/target/debug/agl}}"
execd_bin="${AGL_SMOKE_EXECD_BIN:-$repo_root/target/debug/agl-execd}"
forge_bin="${AGL_SMOKE_FORGE_BIN:-$repo_root/../ayeque-forge/target/debug/ayeque-forge}"
agl_home="${AGL_SMOKE_HOME:-$artifact_root/home-$run_suffix}"
source_root="$agl_home/source"
workspace="$agl_home/workspace"
function_one="$source_root/functions/smoke-one"
function_two="$source_root/functions/smoke-two"
socket="$agl_home/state/daemon/agl.sock"
execd_socket="$agl_home/state/execd/execd.sock"
config="$agl_home/config/agentLIBRE.toml"
daemon_log="$agl_home/daemon.log"
execd_log="$agl_home/execd.log"
model="${AGL_TEST_MODEL_GGUF:?AGL_TEST_MODEL_GGUF must name one exact GGUF model}"
llama_server="${AGL_LLAMA_SERVER_BIN:-$repo_root/target/llama-cpp/build/bin/llama-server}"
python_bin="${AGL_SMOKE_PYTHON:-python3}"
timeout_seconds="${AGL_SMOKE_TIMEOUT_SECONDS:-600}"

for path in "$agl_home" "$model" "$llama_server"; do
  [[ "$path" = /* ]] || {
    printf 'live-smoke path must be absolute: %s\n' "$path" >&2
    exit 2
  }
done
[[ -f "$model" && ! -L "$model" ]] || {
  printf 'GGUF model must be a regular non-symlink file: %s\n' "$model" >&2
  exit 2
}
[[ -x "$llama_server" ]] || {
  printf 'private llama-server is not executable: %s\n' "$llama_server" >&2
  exit 2
}
[[ -x "$forge_bin" ]] || {
  printf 'ayeque-forge is not executable: %s\n' "$forge_bin" >&2
  exit 2
}
for tool in awk cargo cp date git grep head mkdir pgrep sed sleep stat tail timeout; do
  command -v "$tool" >/dev/null || {
    printf '%s is required\n' "$tool" >&2
    exit 2
  }
done
command -v "$python_bin" >/dev/null || {
  printf '%s is required\n' "$python_bin" >&2
  exit 2
}
"$python_bin" -c 'import json, sqlite3' >/dev/null || {
  printf '%s requires working json and sqlite3 modules\n' "$python_bin" >&2
  exit 2
}

cleanup() {
  for child_pid in "${foreground_pids[@]:-}"; do
    if [[ -n "$child_pid" ]] && kill -0 "$child_pid" 2>/dev/null; then
      kill "$child_pid" 2>/dev/null || true
      wait "$child_pid" 2>/dev/null || true
    fi
  done
  if [[ -n "${daemon_pid:-}" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  if [[ -n "${execd_pid:-}" ]] && kill -0 "$execd_pid" 2>/dev/null; then
    kill "$execd_pid" 2>/dev/null || true
    wait "$execd_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

mkdir -p \
  "$agl_home/config" \
  "$agl_home/data/runtime/models" \
  "$agl_home/state/logs" \
  "$source_root/agents/smoke" \
  "$source_root/models/smoke" \
  "$function_one" \
  "$function_two" \
  "$workspace"

export XDG_CONFIG_HOME="$agl_home/config"
export XDG_DATA_HOME="$agl_home/data"

model_sha256="${AGL_TEST_MODEL_SHA256:-$(basename "$model" .gguf)}"
model_sha256="${model_sha256#sha256:}"
[[ "$model_sha256" =~ ^[0-9a-f]{64}$ ]] || {
  printf 'invalid AGL_TEST_MODEL_SHA256\n' >&2
  exit 2
}
model_bytes="$(stat -c %s "$model")"
cache_model="$agl_home/data/runtime/models/$model_sha256.gguf"
# Keep the smoke setup fast for multi-gigabyte GGUFs while still creating an
# independent inode with the managed-file metadata required by the runtime.
cp --reflink=always -- "$model" "$cache_model"
chmod 600 "$cache_model"

SOURCE_ROOT="$source_root" \
REPO_ROOT="$repo_root" \
FUNCTION_ONE="$function_one" \
FUNCTION_TWO="$function_two" \
CONFIG_PATH="$config" \
ENGINE_PATH="$llama_server" \
MODEL_SHA256="$model_sha256" \
MODEL_BYTES="$model_bytes" \
AGL_TEST_MODEL_URL="${AGL_TEST_MODEL_URL:-https://example.invalid/model.gguf}" \
AGL_TEST_MODEL_DIALECT="${AGL_TEST_MODEL_DIALECT:-qwen3}" \
AGL_TEST_TOOL_CALL_FORMAT="${AGL_TEST_TOOL_CALL_FORMAT:-hermes_json}" \
AGL_TEST_CONTEXT_TOKENS="${AGL_TEST_CONTEXT_TOKENS:-4096}" \
AGL_TEST_BATCH_SIZE="${AGL_TEST_BATCH_SIZE:-512}" \
AGL_TEST_UBATCH_SIZE="${AGL_TEST_UBATCH_SIZE:-128}" \
AGL_TEST_THREADS="${AGL_TEST_THREADS:-8}" \
AGL_TEST_GPU_LAYERS="${AGL_TEST_GPU_LAYERS:-0}" \
AGL_TEST_REQUIRED_HOST_BYTES="${AGL_TEST_REQUIRED_HOST_BYTES:-34359738368}" \
AGL_TEST_REQUIRED_DEVICE_BYTES="${AGL_TEST_REQUIRED_DEVICE_BYTES:-0}" \
AGL_TEST_REQUIRED_SHARED_BYTES="${AGL_TEST_REQUIRED_SHARED_BYTES:-0}" \
AGL_TEST_DEVICE="${AGL_TEST_DEVICE:-}" \
AGL_TEST_DEVICE_PATHS="${AGL_TEST_DEVICE_PATHS:-}" \
"$python_bin" - <<'PY'
import json
import os
import stat
from pathlib import Path

source = Path(os.environ["SOURCE_ROOT"])
function_one = Path(os.environ["FUNCTION_ONE"])
function_two = Path(os.environ["FUNCTION_TWO"])
model_digest = "sha256:" + os.environ["MODEL_SHA256"]
context = int(os.environ["AGL_TEST_CONTEXT_TOKENS"])
batch = int(os.environ["AGL_TEST_BATCH_SIZE"])
ubatch = int(os.environ["AGL_TEST_UBATCH_SIZE"])
threads = int(os.environ["AGL_TEST_THREADS"])
gpu_layers = os.environ["AGL_TEST_GPU_LAYERS"].strip()
if gpu_layers == "all":
    gpu_layers_toml = '"all"'
else:
    try:
        gpu_layers_value = int(gpu_layers)
    except ValueError as error:
        raise SystemExit("AGL_TEST_GPU_LAYERS must be a non-negative integer or all") from error
    if gpu_layers_value < 0:
        raise SystemExit("AGL_TEST_GPU_LAYERS must be a non-negative integer or all")
    gpu_layers = str(gpu_layers_value)
    gpu_layers_toml = gpu_layers
device = os.environ["AGL_TEST_DEVICE"] or None
device_paths = [
    str(Path(value).resolve())
    for value in os.environ["AGL_TEST_DEVICE_PATHS"].split(os.pathsep)
    if value
]
for path in device_paths:
    if not stat.S_ISCHR(Path(path).stat().st_mode):
        raise SystemExit(f"inference device path is not a character device: {path}")

(source / "agents/smoke/AGENT.md").write_text(
    "---\n"
    "schema: agentlibre.agent/v1\n"
    "id: agentlibre.smoke-agent\n"
    "version: 1.0.0\n"
    "description: Live smoke Agent\n"
    "required_tools: []\n"
    "---\n",
    encoding="utf-8",
)
(source / "agents/smoke/SYSTEM.md").write_text(
    "Answer briefly in plain text. Do not call tools.\n", encoding="utf-8"
)
(source / "models/smoke/MODEL.toml").write_text(
    "schema = \"agentlibre.model/v1\"\n"
    "id = \"agentlibre.smoke-model\"\n"
    "version = \"1.0.0\"\n"
    "description = \"Live smoke GGUF\"\n"
    f"dialect = \"{os.environ['AGL_TEST_MODEL_DIALECT']}\"\n"
    f"tool_call_format = \"{os.environ['AGL_TEST_TOOL_CALL_FORMAT']}\"\n\n"
    "[artifact]\n"
    "kind = \"gguf\"\n"
    f"url = \"{os.environ['AGL_TEST_MODEL_URL']}\"\n"
    f"sha256 = \"{model_digest}\"\n"
    f"bytes = {os.environ['MODEL_BYTES']}\n",
    encoding="utf-8",
)

def function_document(identifier: str, maximum: int) -> str:
    accelerator = "cpu" if gpu_layers == "0" else "require_gpu"
    return (
        "schema = \"agentlibre.function/v2\"\n"
        f"id = \"{identifier}\"\n"
        "version = \"1.0.0\"\n"
        "agent = { id = \"agentlibre.smoke-agent\", version = \"1.0.0\" }\n"
        "model = { id = \"agentlibre.smoke-model\", version = \"1.0.0\" }\n"
        "extensions = [\n"
        "    { id = \"agentlibre.builtins\", version = \"1.0.0-alpha.19\" },\n"
        "    { id = \"agentlibre.execution\", version = \"1.0.0-alpha.19\" },\n"
        "]\n"
        "working_directory = \".\"\n\n"
        "[presentation.tool_output]\n"
        "lines = 10\n"
        "chars = 500\n"
        "[presentation.tool]\n"
        "frame = true\n"
        "[presentation.colors]\n"
        "rule = \"dim\"\n"
        "run = \"bold #FF00FF\"\n"
        "run_id = \"bold #FFFFFF\"\n"
        "status_success = \"bold #7BD88F\"\n"
        "status_failure = \"bold #FF0000\"\n"
        "status_pending = \"bold #FFFF00\"\n"
        "operation = \"bold #00FFFF\"\n"
        "tool = \"bold #00FFFF\"\n"
        "ordinal = \"dim\"\n"
        "field = \"bold #8AA2D8\"\n"
        "muted = \"dim\"\n"
        "json_key = \"#00FFFF\"\n"
        "json_string = \"#7BD88F\"\n"
        "json_number = \"#FFFF00\"\n"
        "json_boolean = \"#FF00FF\"\n"
        "json_null = \"dim\"\n"
        "markdown_heading = \"bold #00FFFF\"\n"
        "markdown_code = \"dim\"\n"
        "markdown_inline_code = \"#FFFF00\"\n"
        "markdown_strong = \"bold\"\n"
        "markdown_emphasis = \"italic\"\n"
        "markdown_link = \"#8AA2D8\"\n"
        "markdown_quote = \"dim\"\n"
        "markdown_bullet = \"#00FFFF\"\n"
        "markdown_rule = \"dim\"\n"
        "input_rule = \"oklch(0.439 0 0)\"\n"
        "input_background = \"oklch(0.269 0 0)\"\n"
        "input_prompt = \"bold oklch(0.718 0.202 349.761)\"\n"
        "input_hint = \"dim oklch(0.823 0.12 346.018)\"\n"
        "input_text = \"oklch(0.936 0.032 17.717)\"\n"
        "input_activity = \"oklch(0.823 0.12 346.018)\"\n"
        "input_selected = \"bold oklch(0.518 0.253 323.949)\"\n"
        "[presentation.model_generation]\n"
        "details = false\n\n"
        "[permissions]\n"
        "files = \"write\"\n"
        "commands = [\"find\", \"rg\"]\n\n"
        "[inference.generation]\n"
        f"max_output_tokens = {maximum}\n\n"
        "[inference.load]\n"
        f"context_tokens = {context}\n"
        f"batch_size = {batch}\n"
        f"ubatch_size = {ubatch}\n"
        f"threads = {threads}\n"
        f"accelerator = \"{accelerator}\"\n"
        f"gpu_layers = {gpu_layers_toml}\n\n"
        "[inference.service]\n"
        "slots = 2\n"
        "queue_capacity = 8\n"
        "continuous_batching = true\n"
        "idle_timeout = \"15m\"\n"
    )

(function_one / "FUNCTION.toml").write_text(
    function_document("agentlibre.smoke-one", 128), encoding="utf-8"
)
(function_two / "FUNCTION.toml").write_text(
    function_document("agentlibre.smoke-two", 96), encoding="utf-8"
)

Path(os.environ["CONFIG_PATH"]).write_text(
    "[chat]\n"
    "default_function = \"function:agentlibre.smoke-one@^1.0\"\n\n"
    "[inference]\n"
    f"executable = {json.dumps(os.environ['ENGINE_PATH'])}\n",
    encoding="utf-8",
)
PY

git -C "$source_root" init --quiet
git -C "$source_root" config user.name agentLIBRE-smoke
git -C "$source_root" config user.email agentLIBRE-smoke@invalid
git -C "$source_root" add --all
git -C "$source_root" commit --quiet -m 'Create live smoke entities'
source_revision="$(git -C "$source_root" rev-parse HEAD)"

"$forge_bin" init "$workspace" >/dev/null
forge_project="$agl_home/data/ayeque-forge/projects/workspace"
forge_manifest="$forge_project/FORGE.toml"
FORGE_MANIFEST="$forge_manifest" \
SOURCE_ROOT="$source_root" \
SOURCE_REVISION="$source_revision" \
REPO_ROOT="$repo_root" \
REPO_REVISION="$(git -C "$repo_root" rev-parse HEAD)" \
python3 - <<'PY'
import os
from pathlib import Path

manifest = Path(os.environ["FORGE_MANIFEST"])
source = os.environ["SOURCE_ROOT"]
source_revision = os.environ["SOURCE_REVISION"]
repo = os.environ["REPO_ROOT"]
repo_revision = os.environ["REPO_REVISION"]
manifest.write_text(
    "format = 2\n\n"
    "[[entity]]\n"
    "id = \"agentlibre.smoke-agent\"\n"
    "kind = \"agent\"\n"
    "schema = \"agentlibre.agent/v1\"\n"
    f"git = \"{source}\"\n"
    f"revision = \"{source_revision}\"\n"
    "path = \"agents/smoke\"\n\n"
    "[[entity]]\n"
    "id = \"agentlibre.smoke-model\"\n"
    "kind = \"model\"\n"
    "schema = \"agentlibre.model/v1\"\n"
    f"git = \"{source}\"\n"
    f"revision = \"{source_revision}\"\n"
    "path = \"models/smoke\"\n\n"
    "[[entity]]\n"
    "id = \"agentlibre.builtins\"\n"
    "kind = \"extension\"\n"
    "schema = \"agentlibre.extension/v1\"\n"
    f"git = \"{repo}\"\n"
    f"revision = \"{repo_revision}\"\n"
    "path = \"extensions/agentlibre-builtins\"\n\n"
    "[[entity]]\n"
    "id = \"agentlibre.execution\"\n"
    "kind = \"extension\"\n"
    "schema = \"agentlibre.extension/v1\"\n"
    f"git = \"{repo}\"\n"
    f"revision = \"{repo_revision}\"\n"
    "path = \"extensions/agentlibre-execution\"\n\n"
    "[[entity]]\n"
    "id = \"agentlibre.smoke-one\"\n"
    "kind = \"function\"\n"
    "schema = \"agentlibre.function/v2\"\n"
    f"git = \"{source}\"\n"
    f"revision = \"{source_revision}\"\n"
    "path = \"functions/smoke-one\"\n\n"
    "[[entity]]\n"
    "id = \"agentlibre.smoke-two\"\n"
    "kind = \"function\"\n"
    "schema = \"agentlibre.function/v2\"\n"
    f"git = \"{source}\"\n"
    f"revision = \"{source_revision}\"\n"
    "path = \"functions/smoke-two\"\n",
    encoding="utf-8",
)
PY
(cd "$workspace" && "$forge_bin" lock >/dev/null)
(cd "$workspace" && "$forge_bin" validate >/dev/null)
function_one="$agl_home/data/ayeque-forge/projects/workspace/entities/agentlibre.smoke-one"
function_two="$agl_home/data/ayeque-forge/projects/workspace/entities/agentlibre.smoke-two"

if [[ ! -x "$agl_bin" || ! -x "$execd_bin" ]]; then
  cargo build -p agl-cli --bin agl -p agl-execd >/dev/null
fi
AGL_HOME="$agl_home" RUST_LOG="${RUST_LOG:-info}" \
  "$execd_bin" >"$execd_log" 2>&1 &
execd_pid=$!

deadline=$((SECONDS + timeout_seconds))
until [[ -S "$execd_socket" ]]; do
  if ! kill -0 "$execd_pid" 2>/dev/null; then
    printf 'execd exited before socket readiness\n' >&2
    tail -80 "$execd_log" >&2 || true
    exit 1
  fi
  if (( SECONDS >= deadline )); then
    printf 'timed out waiting for execd socket: %s\n' "$execd_socket" >&2
    tail -80 "$execd_log" >&2 || true
    exit 1
  fi
  sleep 0.1
done

if (cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" config apply --function "$function_two") >"$agl_home/config-apply.out" 2>&1; then
  :
else
  config_status=$?
  printf 'config apply failed or timed out (status=%s):\n' "$config_status" >&2
  sed -n '1,120p' "$agl_home/config-apply.out" >&2 || true
  tail -80 "$execd_log" >&2 || true
  exit "$config_status"
fi

AGL_HOME="$agl_home" RUST_LOG="${RUST_LOG:-info}" \
  "$agl_bin" serve >"$daemon_log" 2>&1 &
daemon_pid=$!

deadline=$((SECONDS + timeout_seconds))
until [[ -S "$socket" ]]; do
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    printf 'daemon exited before socket readiness\n' >&2
    tail -80 "$daemon_log" >&2 || true
    exit 1
  fi
  if (( SECONDS >= deadline )); then
    printf 'timed out waiting for daemon socket: %s\n' "$socket" >&2
    tail -80 "$daemon_log" >&2 || true
    exit 1
  fi
  sleep 0.1
done

run_output_file_one="$agl_home/function-one.out"
run_output_file_two="$agl_home/function-two.out"
foreground_pids=()
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" run "$function_one" "Use fs_apply_patch to create tool-proof.txt containing exactly TOOL_EDITED. Then use fs_read to read that file, and command.exec with rg to verify the exact text. Complete all three tool calls before replying with exactly TOOL_DONE.") \
  >"$run_output_file_one" 2>&1 &
run_pid_one=$!
foreground_pids+=("$run_pid_one")
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" run "$function_two" "Respond with exactly the single word SHARED") \
  >"$run_output_file_two" 2>&1 &
run_pid_two=$!
foreground_pids+=("$run_pid_two")
run_status_one=0
run_status_two=0
wait "$run_pid_one" || run_status_one=$?
wait "$run_pid_two" || run_status_two=$?
foreground_pids=()
run_output_one="$(<"$run_output_file_one")"
run_output_two="$(<"$run_output_file_two")"
if (( run_status_one != 0 || run_status_two != 0 )); then
  printf 'concurrent Function Runs failed (%s, %s):\n%s\n%s\n' \
    "$run_status_one" "$run_status_two" "$run_output_one" "$run_output_two" >&2
  exit 1
fi
[[ "$(<"$workspace/tool-proof.txt")" == "TOOL_EDITED" ]] || {
  printf 'Function did not create the expected tool-proof.txt through fs_apply_patch/fs_read\n' >&2
  exit 1
}
[[ "$run_output_one" == *"TOOL_DONE"* ]] || {
  printf 'tool Function did not complete its requested tool sequence:\n%s\n' "$run_output_one" >&2
  exit 1
}
service_one="$(sed -n 's/^model_service=//p' <<<"$run_output_one" | tail -1)"
service_two="$(sed -n 's/^model_service=//p' <<<"$run_output_two" | tail -1)"
if ! { [[ "$service_one" == "new" && "$service_two" == "reused" ]] ||
  [[ "$service_one" == "reused" && "$service_two" == "new" ]]; }; then
  printf 'concurrent Functions did not start and reuse exactly one service:\n%s\n%s\n' \
    "$run_output_one" "$run_output_two" >&2
  exit 1
fi
run_one="$(sed -n 's/^run \(run_[^ ]*\).*/\1/p' <<<"$run_output_one" | tail -1)"
run_two="$(sed -n 's/^run \(run_[^ ]*\).*/\1/p' <<<"$run_output_two" | tail -1)"
[[ "$run_one" == run_* && "$run_two" == run_* && "$run_one" != "$run_two" ]] || {
  printf 'could not identify distinct foreground Runs\n' >&2
  exit 1
}

chat_output="$(cd "$workspace" && printf '%s\n' "Respond with exactly the single word CHAT" | \
  AGL_HOME="$agl_home" timeout "$timeout_seconds" "$agl_bin" 2>&1)"
conversation="$(CHAT_OUTPUT="$chat_output" python3 - <<'PY'
import os
import re

output = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", os.environ["CHAT_OUTPUT"])
match = re.search(
    r"(?:^|\n)(?:conversation|CONVERSATION) (conv_[0-9a-f-]+)",
    output,
)
if match:
    print(match.group(1))
PY
)"
[[ "$conversation" == conv_* ]] || {
  printf 'root chat did not expose a Conversation:\n%s\n' "$chat_output" >&2
  exit 1
}
chat_run="$(STORE_PATH="$agl_home/data/store/agentlibre.sqlite3" \
  CONVERSATION="$conversation" "$python_bin" - <<'PY'
import os
import sqlite3
import uuid

conversation = uuid.UUID(os.environ["CONVERSATION"].removeprefix("conv_")).bytes
connection = sqlite3.connect(f"file:{os.environ['STORE_PATH']}?mode=ro", uri=True)
row = connection.execute(
    "SELECT agent_run_id FROM agent_runs WHERE origin_conversation_id=? ORDER BY admitted_at_ms DESC LIMIT 1",
    (conversation,),
).fetchone()
if row is not None:
    print(f"run_{uuid.UUID(bytes=row[0])}")
PY
)"
[[ "$conversation" == conv_* && "$chat_run" == run_* ]] || {
  printf 'root chat did not expose its Conversation and Run:\n%s\n' "$chat_output" >&2
  exit 1
}

AGL_HOME="$agl_home" "$agl_bin" conversation rename "$conversation" "smoke chat" >/dev/null
resume_output="$(cd "$workspace" && AGL_HOME="$agl_home" "$agl_bin" resume "smoke chat" </dev/null)"
[[ "$resume_output" == *"conversation $conversation"* ||
  "$resume_output" == *"CONVERSATION $conversation"* ]] || {
  printf 'renamed Conversation did not resume\n' >&2
  exit 1
}
last_output="$(cd "$workspace" && AGL_HOME="$agl_home" "$agl_bin" resume --last </dev/null)"
[[ "$last_output" == *"conversation $conversation"* ||
  "$last_output" == *"CONVERSATION $conversation"* ]] || {
  printf 'workspace-local --last did not select the latest Conversation\n' >&2
  exit 1
}

view_one="$(AGL_HOME="$agl_home" "$agl_bin" view "$run_one")"
view_two="$(AGL_HOME="$agl_home" "$agl_bin" view "$run_two")"
view_chat="$(AGL_HOME="$agl_home" "$agl_bin" view "$chat_run")"
for output in "$view_one" "$view_two" "$view_chat"; do
  [[ "$output" == *'"status": "completed"'* ]] || {
    printf 'Run view is not completed:\n%s\n' "$output" >&2
    exit 1
  }
  [[ "$output" != *"$workspace"* && "$output" != *"$model"* ]] || {
    printf 'public Run view leaked a private path\n' >&2
    exit 1
  }
done

STORE_PATH="$agl_home/data/store/agentlibre.sqlite3" \
RUN_ONE="$run_one" RUN_TWO="$run_two" CHAT_RUN="$chat_run" \
CONVERSATION="$conversation" \
"$python_bin" - <<'PY'
import json
import os
import sqlite3

connection = sqlite3.connect(f"file:{os.environ['STORE_PATH']}?mode=ro", uri=True)
runs = connection.execute(
    "SELECT status, count(*) FROM agent_runs GROUP BY status ORDER BY status"
).fetchall()
if runs != [("completed", 3)]:
    raise SystemExit(f"unexpected durable Run outcomes: {runs}")
conversations = connection.execute(
    "SELECT count(*), sum(display_name='smoke chat') FROM agent_conversations"
).fetchone()
if conversations != (3, 1):
    raise SystemExit(f"unexpected durable Conversations: {conversations}")
messages = connection.execute(
    "SELECT role, visibility FROM agent_messages ORDER BY message_sequence"
).fetchall()
if len(messages) != 9:
    raise SystemExit(f"unexpected visible messages: {messages}")
if any(
    visibility != ("internal" if role == "tool" else "conversation")
    for role, visibility in messages
):
    raise SystemExit(f"unexpected message visibility: {messages}")
if [role for role, _ in messages].count("user") != 3 or [role for role, _ in messages].count("assistant") != 3 or [role for role, _ in messages].count("tool") != 3:
    raise SystemExit(f"unexpected message roles: {messages}")
operation_rows = connection.execute(
    "SELECT state, kind, result_json FROM agent_operations ORDER BY agent_run_id, ordinal"
).fetchall()
if not operation_rows or any(state != "succeeded" for state, _, _ in operation_rows):
    raise SystemExit(f"unexpected durable Operations: {operation_rows}")
tool_receipts = 0
model_outputs = []
for _, kind, result_json in operation_rows:
    result = json.loads(result_json)["result"]
    if kind == "tool":
        tool_receipts += len(result.get("effect_receipts", []))
        continue
    model_outputs.append(result.get("output", {}))
    realization = result["realization"]
    for key in ("runtime_profile_digest", "engine_build_digest", "physical_resource_digest"):
        value = realization.get(key, "")
        if not value.startswith("sha256:") or len(value) != 71:
            raise SystemExit(f"invalid realization {key}: {value!r}")
if tool_receipts < 2:
    raise SystemExit(f"expected committed filesystem and command receipts: {tool_receipts}")
if not any(
    output.get("type") == "tool_calls"
    and isinstance(output.get("value"), list)
    and len(output["value"]) >= 2
    for output in model_outputs
):
    raise SystemExit(f"expected a batched model tool-call output: {model_outputs}")
print(f"durable_runs={runs}")
print(f"durable_conversations={conversations[0]}")
print(f"conversation_messages={len(messages)}")
print(f"durable_operations={len(operation_rows)}")
print(f"tool_receipts={tool_receipts}")
PY

child_pid="$(pgrep -P "$execd_pid" | head -1 || true)"
[[ -n "$child_pid" ]] || {
  printf 'no private llama-server child remained under execd after the flows\n' >&2
  exit 1
}
grep -q 'execution service listening' "$execd_log"
grep -q 'Agent daemon services started' "$daemon_log"
grep -q 'private llama-server realization is ready' "$daemon_log"
grep -q 'AgentRun driver reached a terminal state' "$daemon_log"

printf 'function_one_run=%s\n' "$run_one"
printf 'function_two_run=%s\n' "$run_two"
printf 'chat_conversation=%s\n' "$conversation"
printf 'chat_run=%s\n' "$chat_run"
printf 'inference_child_pid=%s\n' "$child_pid"
printf 'smoke_home=%s\n' "$agl_home"
printf 'daemon_log=%s\n' "$daemon_log"
printf 'execd_log=%s\n' "$execd_log"
