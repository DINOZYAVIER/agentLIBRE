#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

fail() {
  printf 'agl-init-live-smoke: %s\n' "$*" >&2
  exit 1
}

need_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "required tool is not available: $1"
}

usage() {
  cat <<'EOF'
Opt-in AGL-144 native setup smoke. This can download multi-gigabyte GGUF files.

Required:
  AGL_INIT_SMOKE_ALLOW_DOWNLOAD=1

Useful overrides:
  AGL_INIT_SMOKE_MODEL=gemma4-e2b|gemma4-e4b|gemma4-12b|gemma4-26b|gemma4-31b
  AGL_INIT_SMOKE_AGL_BIN=/path/to/agl
  AGL_INIT_SMOKE_ROOT=/path/to/new/evidence-directory
  AGL_INIT_SMOKE_ALLOW_LOW_MEMORY=1
  AGL_INIT_SMOKE_INITIAL_OFFLINE=1
  AGL_INIT_SMOKE_INTERRUPT_AFTER_SECONDS=N
  AGL_INIT_SMOKE_INTERRUPT_SIGNAL=INT|TERM
  AGL_INIT_SMOKE_REQUIRE_CPU=1
  AGL_INIT_SMOKE_REQUIRE_GPU=1
  AGL_INIT_SMOKE_TOOL_FIXTURE=0
  AGL_INIT_SMOKE_SKIP_BUILD=1

The workspace and AGL home are isolated. HF_HOME/HF_HUB_CACHE are deliberately
not changed, so the standard Hugging Face cache and authentication settings are
used. Token values are never recorded.
EOF
}

redact_file() {
  local path="$1"
  python3 - "$path" <<'PY'
import os
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
if not path.exists():
    raise SystemExit(0)
text = path.read_text(encoding="utf-8", errors="replace")
for name in ("HF_TOKEN",):
    value = os.environ.get(name)
    if value:
        text = text.replace(value, "[REDACTED]")
path.write_text(text, encoding="utf-8")
PY
}

record_command() {
  local label="$1"
  shift
  local stdout_path="$evidence_root/$label.stdout"
  local stderr_path="$evidence_root/$label.stderr"
  local time_path="$evidence_root/$label.time"
  local status

  {
    printf '%s\t' "$label"
    printf '%q ' "$@"
    printf '\n'
  } >>"$evidence_root/commands.log"

  set +e
  (
    cd "$workspace"
    "$time_bin" -v -o "$time_path" "$@" >"$stdout_path" 2>"$stderr_path"
  )
  status=$?
  set -e
  redact_file "$stdout_path"
  redact_file "$stderr_path"
  printf '%s\t%s\n' "$label" "$status" >>"$evidence_root/results.tsv"
  return "$status"
}

require_json_state() {
  local path="$1"
  local expected="$2"
  python3 - "$path" "$expected" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected = sys.argv[2]
with path.open(encoding="utf-8") as handle:
    report = json.load(handle)
actual = report.get("state")
if actual != expected:
    raise SystemExit(f"{path}: expected state {expected!r}, got {actual!r}")
PY
}

require_catalog_and_runtime() {
  local path="$1"
  local selected="$2"
  local require_cpu="$3"
  local require_gpu="$4"
  python3 - "$path" "$selected" "$require_cpu" "$require_gpu" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
selected = sys.argv[2]
require_cpu = sys.argv[3] == "1"
require_gpu = sys.argv[4] == "1"
with path.open(encoding="utf-8") as handle:
    report = json.load(handle)
plan = report["plan"]
if plan["selected_package"] != selected:
    raise SystemExit(f"selected package mismatch: {plan['selected_package']!r}")
package_ids = {item["package_id"] for item in plan["packages"]}
required = {"gemma4-e2b", "gemma4-e4b", "gemma4-12b", "gemma4-26b", "gemma4-31b"}
if package_ids != required:
    raise SystemExit(f"catalog mismatch: {sorted(package_ids)!r}")
gpu_layers = plan["runtime"]["selected"]["runtime"]["gpu_layers"]
if require_cpu and gpu_layers != 0:
    raise SystemExit(f"CPU-only smoke selected {gpu_layers} GPU layers")
if require_gpu and gpu_layers == 0:
    raise SystemExit("GPU smoke selected the CPU plan")
PY
}

if [[ "${AGL_INIT_SMOKE_ALLOW_DOWNLOAD:-0}" != "1" ]]; then
  usage >&2
  fail "set AGL_INIT_SMOKE_ALLOW_DOWNLOAD=1 to acknowledge model downloads"
fi

need_tool cargo
need_tool git
need_tool grep
need_tool python3
need_tool realpath
time_bin="$(type -P time || true)"
[[ -n "$time_bin" && -x "$time_bin" ]] || fail "required executable is not available: time"

model="${AGL_INIT_SMOKE_MODEL:-gemma4-e4b}"
case "$model" in
  gemma4-e2b | gemma4-e4b | gemma4-12b | gemma4-26b | gemma4-31b) ;;
  *) fail "unsupported AGL_INIT_SMOKE_MODEL: $model" ;;
esac
case "$model" in
  gemma4-e4b | gemma4-12b) projector_model="${model}-mmproj" ;;
  gemma4-e2b | gemma4-26b | gemma4-31b) projector_model="" ;;
esac

require_cpu="${AGL_INIT_SMOKE_REQUIRE_CPU:-0}"
require_gpu="${AGL_INIT_SMOKE_REQUIRE_GPU:-0}"
[[ "$require_cpu" == "0" || "$require_cpu" == "1" ]] ||
  fail "AGL_INIT_SMOKE_REQUIRE_CPU must be 0 or 1"
[[ "$require_gpu" == "0" || "$require_gpu" == "1" ]] ||
  fail "AGL_INIT_SMOKE_REQUIRE_GPU must be 0 or 1"
[[ "$require_cpu" != "1" || "$require_gpu" != "1" ]] ||
  fail "CPU-only and GPU-required modes are mutually exclusive"

if [[ -n "${AGL_INIT_SMOKE_ROOT:-}" ]]; then
  artifact_root="$AGL_INIT_SMOKE_ROOT"
  [[ ! -e "$artifact_root" ]] || fail "evidence root already exists: $artifact_root"
  mkdir -p "$artifact_root"
else
  artifact_root="$(mktemp -d "${TMPDIR:-/tmp}/agl-144-init-live.XXXXXX")"
fi
artifact_root="$(realpath "$artifact_root")"
workspace="$artifact_root/workspace"
agl_home="$artifact_root/agl-home"
evidence_root="$artifact_root/evidence"
mkdir -p "$workspace" "$agl_home" "$evidence_root"
git -C "$workspace" init -q

agl_bin="${AGL_INIT_SMOKE_AGL_BIN:-$repo_root/target/release/agl}"
if [[ "${AGL_INIT_SMOKE_SKIP_BUILD:-0}" != "1" ]]; then
  (
    cd "$repo_root"
    cargo build --locked --release \
      -p agl-cli \
      -p agl-process \
      --bin agl \
      --bin agl-process-launcher
  )
fi
[[ -x "$agl_bin" ]] || fail "agl binary is not executable: $agl_bin"
agl_bin="$(realpath "$agl_bin")"

{
  printf 'schema=agentlibre.init_live_smoke.v1\n'
  printf 'started_at=%s\n' "$(date --iso-8601=seconds)"
  printf 'model=%s\n' "$model"
  printf 'agl_bin=%s\n' "$agl_bin"
  printf 'workspace=%s\n' "$workspace"
  printf 'agl_home=%s\n' "$agl_home"
  printf 'hf_home_override=%s\n' "${HF_HOME:+configured}"
  printf 'hf_hub_cache_override=%s\n' "${HF_HUB_CACHE:+configured}"
  printf 'hf_token=%s\n' "${HF_TOKEN:+configured}"
  printf 'hf_token_path=%s\n' "${HF_TOKEN_PATH:+configured}"
  printf 'require_cpu=%s\n' "$require_cpu"
  printf 'require_gpu=%s\n' "$require_gpu"
  uname -a
} >"$evidence_root/diagnostics.txt"
printf 'case\texit_status\n' >"$evidence_root/results.tsv"

init_args=(
  "$agl_bin" --home "$agl_home" init --model "$model"
  --yes --non-interactive --json
)
if [[ "${AGL_INIT_SMOKE_ALLOW_LOW_MEMORY:-0}" == "1" ]]; then
  init_args+=(--allow-low-memory)
fi

plan_args=("${init_args[@]}" --dry-run)
if [[ "${AGL_INIT_SMOKE_INITIAL_OFFLINE:-0}" == "1" ]]; then
  plan_args+=(--offline)
fi
record_command plan "${plan_args[@]}" || fail "setup plan failed; see $evidence_root/plan.stderr"
require_json_state "$evidence_root/plan.stdout" planned
require_catalog_and_runtime "$evidence_root/plan.stdout" "$model" "$require_cpu" "$require_gpu"

interrupt_after="${AGL_INIT_SMOKE_INTERRUPT_AFTER_SECONDS:-0}"
if [[ "$interrupt_after" != "0" ]]; then
  [[ "$interrupt_after" =~ ^[1-9][0-9]*$ ]] ||
    fail "AGL_INIT_SMOKE_INTERRUPT_AFTER_SECONDS must be a positive integer"
  interrupt_signal="${AGL_INIT_SMOKE_INTERRUPT_SIGNAL:-INT}"
  case "$interrupt_signal" in
    INT | TERM) ;;
    *) fail "AGL_INIT_SMOKE_INTERRUPT_SIGNAL must be INT or TERM" ;;
  esac
  need_tool timeout
  if record_command interrupted timeout \
    --signal="$interrupt_signal" \
    --kill-after=15s \
    "${interrupt_after}s" \
    "${init_args[@]}"; then
    fail "interrupted setup completed before the signal; use a fresh cache or shorter delay"
  fi
  shopt -s nullglob
  checkpoints=("$agl_home/state/setup"/*.json)
  shopt -u nullglob
  [[ ${#checkpoints[@]} -eq 1 ]] ||
    fail "interrupted setup did not preserve exactly one workspace checkpoint"
fi

first_args=("${init_args[@]}")
if [[ "${AGL_INIT_SMOKE_INITIAL_OFFLINE:-0}" == "1" ]]; then
  first_args+=(--offline)
fi
record_command first-init "${first_args[@]}" ||
  fail "first setup failed; see $evidence_root/first-init.stderr"
require_json_state "$evidence_root/first-init.stdout" ready
[[ -f "$workspace/.agl/workspace.toml" ]] || fail "setup did not publish workspace manifest"
[[ -f "$agl_home/config/models.toml" ]] || fail "setup did not publish models.toml"
shopt -s nullglob
remaining_checkpoints=("$agl_home/state/setup"/*.json)
shopt -u nullglob
[[ ${#remaining_checkpoints[@]} -eq 0 ]] || fail "completed setup retained a checkpoint"

record_command model-list "$agl_bin" --home "$agl_home" model list --json ||
  fail "model list failed"
record_command verify-main "$agl_bin" --home "$agl_home" model verify "$model" --json ||
  fail "main model verification failed"
if [[ -n "$projector_model" ]]; then
  record_command verify-projector \
    "$agl_bin" --home "$agl_home" model verify "$projector_model" --json ||
    fail "projector verification failed"
fi

record_command text-generation \
  "$agl_bin" --home "$agl_home" run \
  --function "$model" \
  --workspace-root "$workspace" \
  --max-output-tokens 32 \
  --prompt "Reply with exactly: AGL144_TEXT_OK" ||
  fail "normal function generation failed"
grep -q 'AGL144_TEXT_OK' "$evidence_root/text-generation.stdout" ||
  fail "normal function generation did not return the expected marker"

if [[ "${AGL_INIT_SMOKE_TOOL_FIXTURE:-1}" == "1" ]]; then
  printf '%s\n' 'AGL144_TOOL_FIXTURE' >"$workspace/facts.txt"
  tool_prompt='Your first response must be only this exact native Gemma tool call:
<|tool_call>call:fs.read{path:<|"|>facts.txt<|"|>,limit_lines:20}<tool_call|>
After the tool observation, call no more tools and answer with exactly: AGL144_TOOL_OK'
  record_command tool-generation \
    "$agl_bin" --home "$agl_home" run \
    --function "$model" \
    --workspace-root "$workspace" \
    --tool-mode read-only \
    --max-output-tokens 96 \
    --prompt "$tool_prompt" ||
    fail "native Gemma tool fixture failed"
  grep -q 'AGL144_TOOL_OK' "$evidence_root/tool-generation.stdout" ||
    fail "native Gemma tool fixture did not return the expected marker"
else
  printf 'tool-generation\tskipped\n' >>"$evidence_root/results.tsv"
fi

if record_command active-unbind-refusal \
  "$agl_bin" --home "$agl_home" model unbind "$model" --yes; then
  fail "active default model was unexpectedly unbound"
fi
grep -qi 'active\|default\|protected' "$evidence_root/active-unbind-refusal.stderr" ||
  fail "active unbind refusal did not explain the protected reference"

repeat_args=("${init_args[@]}" --offline)
record_command repeat-init "${repeat_args[@]}" ||
  fail "completed offline re-entry failed; see $evidence_root/repeat-init.stderr"
require_json_state "$evidence_root/repeat-init.stdout" ready
if grep -q '^Downloading ' "$evidence_root/repeat-init.stderr"; then
  fail "completed offline re-entry unexpectedly entered model transfer"
fi

{
  printf 'completed_at=%s\n' "$(date --iso-8601=seconds)"
  printf 'result=passed\n'
} >>"$evidence_root/diagnostics.txt"

printf 'AGL-144 init live smoke passed\n'
printf 'Evidence: %s\n' "$evidence_root"
