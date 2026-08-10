#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
agent_root="$(cd -- "$script_dir/../.." && pwd)"
terminal_root="${AGL_TERMINAL_REPO:-$(cd -- "$agent_root/../agl-terminal" && pwd)}"
temporary_root="$(mktemp -d)"
terminal_pid=""
agent_pid=""
cleanup() {
  [[ -z "$agent_pid" ]] || kill "$agent_pid" 2>/dev/null || true
  [[ -z "$terminal_pid" ]] || kill "$terminal_pid" 2>/dev/null || true
  [[ -z "$agent_pid" ]] || wait "$agent_pid" 2>/dev/null || true
  [[ -z "$terminal_pid" ]] || wait "$terminal_pid" 2>/dev/null || true
  chmod -R u+w "$temporary_root" 2>/dev/null || true
  rm -rf -- "$temporary_root"
}
report_failure() {
  local status=$?
  printf 'agl151-session-cli: failed with status=%s\n' "$status" >&2
  [[ ! -f "$temporary_root/terminald.log" ]] || sed -n '1,240p' "$temporary_root/terminald.log" >&2
  [[ ! -f "$temporary_root/agent.log" ]] || sed -n '1,240p' "$temporary_root/agent.log" >&2
  exit "$status"
}
trap cleanup EXIT
trap report_failure ERR

cargo build --locked --manifest-path "$agent_root/Cargo.toml" -p agl-cli --bin agl
cargo build --locked --manifest-path "$terminal_root/Cargo.toml" \
  -p agl-terminald --bin agl-terminald \
  -p agl-process-launcher --bin agl-process-launcher

agl="$agent_root/target/debug/agl"
terminald="$terminal_root/target/debug/agl-terminald"
launcher="$terminal_root/target/debug/agl-process-launcher"
agl_home="$temporary_root/agent-home"
workspace="$temporary_root/workspace"
terminal_socket="$agl_home/runtime/agl-terminal/terminal.sock"
terminal_state="$agl_home/agl-terminal"
terminal_data="$temporary_root/terminal-data"
agent_socket="$agl_home/runtime/agentLIBRE/daemon/agl.sock"
inference_config="$temporary_root/inference.toml"
mkdir -p "$workspace" "$(dirname -- "$terminal_socket")" "$(dirname -- "$agent_socket")"
chmod 0700 "$(dirname -- "$terminal_socket")" "$(dirname -- "$agent_socket")"
mkdir -p "$workspace/.agl/functions/session-smoke"
printf '%s\n' \
  '---' \
  'artifact:' \
  '  schema: agentlibre.package/v1' \
  '  type: function' \
  '  id: session-smoke' \
  '  version: 1.0.0' \
  '  payload_schema: agentlibre.function/v2' \
  '  agl:' \
  '    compatible: ">=1.0.0-alpha.12"' \
  '    tested: [1.0.0-alpha.12]' \
  '  requires:' \
  '    - extension:core.workspace@^1.0' \
  'title: Session CLI smoke' \
  'runtime:' \
  '  tool_mode: read-only' \
  '  max_output_tokens: 16' \
  'skills:' \
  '  use: []' \
  'subagents:' \
  '  use: []' \
  '---' >"$workspace/.agl/functions/session-smoke/FUNCTION.md"
printf '%s\n' 'Session lifecycle verification.' \
  >"$workspace/.agl/functions/session-smoke/SYSTEM.md"
printf '[backend]\nkind = "llama_cpp"\nmodel = "%s"\n\n[runtime]\ngpu_layers = 0\ncontext_tokens = 128\nthreads = 1\nbatch_size = 16\nubatch_size = 16\n\n[model]\ndialect = "qwen3"\ntool_call_format = "hermes_json"\n' \
  "$temporary_root/unused-smoke-model.gguf" >"$inference_config"

unavailable="$temporary_root/bare-unavailable.txt"
AGL_HOME="$agl_home" "$agl" >"$unavailable"
grep -Fx 'interactive_ui=agl-terminal' "$unavailable" >/dev/null
grep -Fx 'state=not_running' "$unavailable" >/dev/null
[[ ! -e "$agl_home/data/sessions" ]] || {
  printf 'agl151-session-cli: bare agl created canonical session state\n' >&2
  exit 1
}

terminal_build_id="$(sed -n '/^pub const TERMINAL_BUILD_ID: &str =/{n;s/^[[:space:]]*"\([^"]*\)";$/\1/p;}' \
  "$agent_root/crates/agl-process/src/lib.rs")"
[[ "$terminal_build_id" == sha256:* ]] || {
  printf 'agl151-session-cli: could not read terminal build identity\n' >&2
  exit 1
}

AGL_TERMINALD_SOCKET="$terminal_socket" \
AGL_TERMINALD_LAUNCHER="$launcher" \
AGL_TERMINALD_DATA_ROOT="$terminal_data" \
AGL_TERMINALD_STATE_ROOT="$terminal_state" \
AGL_TERMINALD_BUILD_ID="$terminal_build_id" \
  "$terminald" >"$temporary_root/terminald.log" 2>&1 &
terminal_pid=$!
for _ in $(seq 1 200); do
  [[ -S "$terminal_socket" ]] && break
  kill -0 "$terminal_pid" 2>/dev/null || {
    sed -n '1,200p' "$temporary_root/terminald.log" >&2
    exit 1
  }
  sleep 0.025
done
[[ -S "$terminal_socket" ]] || {
  printf 'agl151-session-cli: terminal socket was not created\n' >&2
  exit 1
}

AGL_HOME="$agl_home" "$agl" serve --socket "$agent_socket" \
  --config "$inference_config" --function session-smoke --workspace-root "$workspace" \
  --max-output-tokens 16 --tool-mode read-only \
  >"$temporary_root/agent.log" 2>&1 &
agent_pid=$!
for _ in $(seq 1 400); do
  [[ -S "$agent_socket" ]] && break
  kill -0 "$agent_pid" 2>/dev/null || {
    sed -n '1,200p' "$temporary_root/agent.log" >&2
    exit 1
  }
  sleep 0.025
done
[[ -S "$agent_socket" ]] || {
  printf 'agl151-session-cli: agent socket was not created\n' >&2
  exit 1
}

run_agl() {
  AGL_HOME="$agl_home" "$agl" --socket "$agent_socket" "$@"
}

printf 'agl151-session-cli: checking empty daemon state and bare handoff\n'
before_json="$(run_agl session list --json)"
[[ "$before_json" == *'"sessions": []'* ]] || {
  printf 'agl151-session-cli: fresh daemon did not start with an empty session list\n' >&2
  exit 1
}
available="$(run_agl)"
grep -Fx 'daemon=running' <<<"$available" >/dev/null
grep -Fx 'session_count=0' <<<"$available" >/dev/null
[[ "$(run_agl session list --json)" == "$before_json" ]] || {
  printf 'agl151-session-cli: bare agl mutated daemon session state\n' >&2
  exit 1
}

printf 'agl151-session-cli: checking new/list/show/resume/follow\n'
opened="$(run_agl session new --workspace-root "$workspace" --json)"
session_id="$(sed -n 's/.*"session_id": "\([^"]*\)".*/\1/p' <<<"$opened" | head -1)"
[[ "$session_id" == ses_* ]] || {
  printf 'agl151-session-cli: session new returned no canonical SessionId\n' >&2
  exit 1
}
run_agl session list --json | grep -F "\"$session_id\"" >/dev/null
run_agl session show "$session_id" --include-content --json | grep -F '"transcript"' >/dev/null
run_agl session resume "$session_id" --json | grep -F "\"$session_id\"" >/dev/null

run_agl session follow "$session_id" --json >"$temporary_root/follow.jsonl" &
follow_pid=$!
printf 'agl151-session-cli: checking submit/cancel/finish\n'
accepted="$(run_agl session submit "$session_id" --json 'bounded smoke prompt')"
grep -F '"run_id"' <<<"$accepted" >/dev/null
run_agl session cancel "$session_id" --json | grep -F "\"$session_id\"" >/dev/null
run_agl session finish "$session_id" --json | grep -F "\"$session_id\"" >/dev/null
for _ in $(seq 1 2000); do
  ! kill -0 "$follow_pid" 2>/dev/null && break
  sleep 0.025
done
if kill -0 "$follow_pid" 2>/dev/null; then
  printf 'agl151-session-cli: follow did not finish with its durable session\n' >&2
  exit 1
fi
wait "$follow_pid"
[[ -s "$temporary_root/follow.jsonl" ]] || {
  printf 'agl151-session-cli: follow returned no bounded presentation events\n' >&2
  exit 1
}
run_agl session resume "$session_id" --json >"$temporary_root/resume-finished.out" 2>&1 || true
listed_ids="$(run_agl session list --json | grep -o 'ses_[0-9a-f-]*' | sort -u)"
[[ "$listed_ids" == "$session_id" ]] || {
  printf 'agl151-session-cli: resume of a finished session created a replacement identity\n' >&2
  exit 1
}

printf 'agl151-session-cli: passed session=%s\n' "$session_id"
