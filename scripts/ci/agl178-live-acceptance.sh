#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"
cd "$repo_root"

fail() {
  printf 'agl178-live: %s\n' "$*" >&2
  exit 1
}

[[ "${AGL178_LIVE_APPLY:-}" == 1 ]] ||
  fail "set AGL178_LIVE_APPLY=1 to authorize installed user-systemd acceptance"
: "${AGL_TEST_MODEL_GGUF:?AGL_TEST_MODEL_GGUF must be the exact Gemma 4 31B GGUF}"
[[ -f "$AGL_TEST_MODEL_GGUF" && ! -L "$AGL_TEST_MODEL_GGUF" ]] ||
  fail "model is not one regular file: $AGL_TEST_MODEL_GGUF"
[[ "$(stat -c '%s' -- "$AGL_TEST_MODEL_GGUF")" == 17651001568 ]] ||
  fail "model size does not match model:gemma4-31b@1.3.0"
[[ "$(sha256sum -- "$AGL_TEST_MODEL_GGUF" | awk '{print $1}')" == \
  179cfb99212709597eae5929112cfca677e1bbf566178b479ae1da0c4772874b ]] ||
  fail "model digest does not match model:gemma4-31b@1.3.0"
[[ -z "$(git status --porcelain=v1 --untracked-files=normal --ignore-submodules=none)" ]] ||
  fail "live installation requires a clean root workspace"
systemctl --user show-environment >/dev/null ||
  fail "a live user systemd manager is required"

live_root="$(mktemp -d)"
terminal_prefix="$live_root/terminal-prefix"
agent_root="$live_root/agent-root"
agl_home="$live_root/agl-home"
unit_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"
unit_dir="$unit_home/systemd/user"
backup="$live_root/unit-backup"
mkdir -p "$backup" "$unit_dir" "$agl_home/runtime"
chmod 0700 "$agl_home" "$agl_home/runtime"

units=(
  agl-terminald.service
  agl-terminald.socket
  agentlibre-daemon.service
  agentlibre-daemon.socket
)
declare -A was_enabled=()
declare -A was_active=()
for unit in "${units[@]}"; do
  if systemctl --user is-enabled --quiet "$unit" 2>/dev/null; then
    was_enabled["$unit"]=1
  else
    was_enabled["$unit"]=0
  fi
  if systemctl --user is-active --quiet "$unit" 2>/dev/null; then
    was_active["$unit"]=1
  else
    was_active["$unit"]=0
  fi
  if [[ -e "$unit_dir/$unit" || -L "$unit_dir/$unit" ]]; then
    [[ "${AGL178_LIVE_REPLACE_UNITS:-}" == 1 ]] ||
      fail "$unit already exists; set AGL178_LIVE_REPLACE_UNITS=1 to back it up temporarily"
    cp -a -- "$unit_dir/$unit" "$backup/$unit"
  fi
done
if [[ -e "$unit_home/agl-terminal/service.env" ]]; then
  [[ "${AGL178_LIVE_REPLACE_UNITS:-}" == 1 ]] ||
    fail "terminal service.env already exists; set AGL178_LIVE_REPLACE_UNITS=1 to back it up temporarily"
  mkdir -p "$backup/agl-terminal"
  cp -a -- "$unit_home/agl-terminal/service.env" "$backup/agl-terminal/service.env"
fi
for unit in "${units[@]}"; do
  rm -f -- "$unit_dir/$unit"
done
rm -f -- "$unit_home/agl-terminal/service.env"

cleanup() {
  local status=$?
  trap - EXIT
  systemctl --user disable --now agentlibre-daemon.socket agl-terminald.socket \
    >/dev/null 2>&1 || true
  systemctl --user stop agentlibre-daemon.service agl-terminald.service \
    >/dev/null 2>&1 || true
  for unit in "${units[@]}"; do
    rm -f -- "$unit_dir/$unit"
    if [[ -e "$backup/$unit" || -L "$backup/$unit" ]]; then
      cp -a -- "$backup/$unit" "$unit_dir/$unit"
    fi
  done
  rm -f -- "$unit_home/agl-terminal/service.env"
  if [[ -e "$backup/agl-terminal/service.env" ]]; then
    mkdir -p "$unit_home/agl-terminal"
    cp -a -- "$backup/agl-terminal/service.env" \
      "$unit_home/agl-terminal/service.env"
  fi
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  for unit in "${units[@]}"; do
    if [[ "${was_enabled[$unit]}" == 1 ]]; then
      systemctl --user enable "$unit" >/dev/null 2>&1 || true
    fi
  done
  for unit in "${units[@]}"; do
    if [[ "${was_active[$unit]}" == 1 ]]; then
      systemctl --user start "$unit" >/dev/null 2>&1 || true
    fi
  done
  chmod -R u+w "$live_root" 2>/dev/null || true
  rm -rf -- "$live_root"
  exit "$status"
}
trap cleanup EXIT

terminal_output="$(scripts/terminal/install.sh --prefix "$terminal_prefix")"
terminal_generation="$(sed -n 's/^generation=//p' <<<"$terminal_output")"
[[ -d "$terminal_generation" ]] || fail "terminal installer returned no generation"

terminal_env=(
  "XDG_CONFIG_HOME=$unit_home"
  "XDG_DATA_HOME=$agl_home/data"
  "XDG_STATE_HOME=$agl_home/state"
  "XDG_RUNTIME_DIR=$agl_home/runtime"
)
env "${terminal_env[@]}" scripts/terminal/systemd-user-service.sh \
  --prefix "$terminal_prefix" --apply --enable --restart >/dev/null

AGL_HOME="$agl_home" scripts/install-agl-cargo.sh \
  --root "$agent_root" \
  --terminal-generation "$terminal_generation" \
  --skip-submodules >/dev/null
agl="$agent_root/bin/agl"
[[ -x "$agl" ]] || fail "agent installer returned no public command"

AGL_HOME="$agl_home" "$agl" model import "$AGL_TEST_MODEL_GGUF" \
  --id gemma4-31b --replace --json >"$live_root/model-import.json"

AGL_HOME="$agl_home" \
XDG_CONFIG_HOME="$unit_home" \
  scripts/agentlibre-daemon-systemd-service.sh \
    --binary "$agl" \
    --socket "$agl_home/state/daemon/agl.sock" \
    --workspace-root "$repo_root" \
    --function gemma4-31b-32k \
    --tool-mode execute \
    --max-output-tokens 256 \
    --enable --restart >/dev/null

systemctl --user stop agentlibre-daemon.service agl-terminald.service
rm -f -- "$agl_home/runtime/agl-terminal/service-identity.json"

AGL_HOME="$agl_home" "$agl" >"$live_root/bare.out"
grep -F 'daemon=running' "$live_root/bare.out" >/dev/null ||
  fail "bare agl did not cold-activate the installed pair"
[[ -f "$agl_home/runtime/agl-terminal/service-identity.json" ]] ||
  fail "cold activation published no terminal runtime identity"

AGL_HOME="$agl_home" "$agl" runtime identity >"$live_root/runtime-identity.json"
grep -F '"schema": "agentlibre.runtime-identity/v2"' \
  "$live_root/runtime-identity.json" >/dev/null ||
  fail "installed agent runtime identity is not v2"
grep -F '"kind": "sealed"' "$live_root/runtime-identity.json" >/dev/null ||
  fail "agent runtime is not a sealed installed generation"
grep -F '"terminal_generation"' "$live_root/runtime-identity.json" >/dev/null ||
  fail "agent runtime identity omitted the terminal pair"

session_json="$(AGL_HOME="$agl_home" "$agl" session new \
  --workspace-root "$repo_root" --json)"
session_id="$(sed -n 's/.*"session_id": "\([^"]*\)".*/\1/p' <<<"$session_json" | head -n1)"
[[ "$session_id" == ses_* ]] || fail "session new returned no typed SessionId"
AGL_HOME="$agl_home" "$agl" session list --json | grep -F "$session_id" >/dev/null
AGL_HOME="$agl_home" "$agl" session finish "$session_id" --json >/dev/null

AGL_HOME="$agl_home" "$agl" run \
  --function gemma4-31b-32k \
  --tool-mode execute \
  --max-output-tokens 256 \
  --prompt 'Use the process tool to run printf AGL178_PROCESS_OK, then report its exact output.' \
  >"$live_root/run.out"
grep -R -a -F 'AGL178_PROCESS_OK' \
  "$agl_home/state/agl-terminal/spool" >/dev/null ||
  fail "installed Function produced no terminal process-effect evidence"

attempt_root="$agl_home/state/inference/attempts"
[[ -d "$attempt_root" ]] || fail "installed inference produced no attempt journal"
grep -R -F '"selected_device":"Vulkan0"' "$attempt_root" >/dev/null ||
  grep -R -F '"selected_device": "Vulkan0"' "$attempt_root" >/dev/null ||
  fail "inference evidence does not select Vulkan0"
grep -R -E '"device_bytes"[[:space:]]*:[[:space:]]*[1-9][0-9]*' \
  "$attempt_root" >/dev/null ||
  fail "inference evidence contains no positive GPU allocation"
if grep -R -E '"selected_device"[[:space:]]*:[[:space:]]*"CPU"' \
  "$attempt_root" >/dev/null; then
  fail "inference evidence contains a CPU fallback"
fi

before_process="$(sed -n 's/.*"process_generation_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$agl_home/runtime/agl-terminal/service-identity.json")"
systemctl --user restart agl-terminald.service
systemctl --user restart agentlibre-daemon.service
AGL_HOME="$agl_home" "$agl" daemon status >/dev/null
after_process="$(sed -n 's/.*"process_generation_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$agl_home/runtime/agl-terminal/service-identity.json")"
[[ -n "$before_process" && -n "$after_process" && "$before_process" != "$after_process" ]] ||
  fail "terminal restart did not rotate the live process generation"
[[ -d "$agl_home/data/sessions/$session_id" ]] ||
  fail "restart lost durable session state"

printf 'agl178-live: passed terminal activation, process effect and 32K Vulkan inference\n'
