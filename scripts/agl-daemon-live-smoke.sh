#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

config="${1:-${AGL_SMOKE_CONFIG:-}}"
[[ -n "$config" ]] || {
  echo "usage: scripts/agl-daemon-live-smoke.sh /path/to/local-inference.toml" >&2
  echo "set AGL_SMOKE_SERVE_MODE=function|inference to select the daemon surface" >&2
  exit 2
}

artifact_root="${AGL_SMOKE_ARTIFACT_ROOT:-/tmp/agl-073-daemon-smoke}"
run_suffix="agl-073-$(date +%s)-$$"
agl_bin="${AGL_SMOKE_AGL_BIN:-${AGL_BIN:-$repo_root/target/debug/agl}}"
home="${AGL_SMOKE_HOME:-$artifact_root/home-$run_suffix}"
socket="$home/state/daemon/agl.sock"
daemon_log="$home/daemon.log"
prompt="${AGL_SMOKE_PROMPT:-Say 'daemon smoke ok' in one short sentence.}"
max_tokens="${AGL_SMOKE_MAX_OUTPUT_TOKENS:-64}"
timeout_seconds="${AGL_SMOKE_TIMEOUT_SECONDS:-180}"
serve_mode="${AGL_SMOKE_SERVE_MODE:-function}"

case "$serve_mode" in
  function)
    serve_command=(serve)
    ;;
  inference)
    serve_command=(inference serve)
    ;;
  *)
    echo "unsupported AGL_SMOKE_SERVE_MODE: $serve_mode" >&2
    exit 2
    ;;
esac

cleanup() {
  if [[ -n "${daemon_pid:-}" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

mkdir -p "$home"
cargo build -p agl-cli >/dev/null

"$agl_bin" \
  --home "$home" \
  "${serve_command[@]}" \
  --socket "$socket" \
  --config "$config" \
  --max-output-tokens "$max_tokens" \
  >"$daemon_log" 2>&1 &
daemon_pid=$!

deadline=$((SECONDS + timeout_seconds))
while true; do
  if ! status_output="$("$agl_bin" --home "$home" daemon status --socket "$socket" 2>&1)"; then
    echo "daemon status probe failed" >&2
    printf '%s\n' "$status_output" >&2
    exit 1
  fi
  if printf '%s\n' "$status_output" | grep -q '^state=running$'; then
    break
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    echo "daemon exited before becoming ready" >&2
    cat "$daemon_log" >&2 || true
    exit 1
  fi
  if (( SECONDS >= deadline )); then
    echo "timed out waiting for daemon socket: $socket" >&2
    cat "$daemon_log" >&2 || true
    exit 1
  fi
  sleep 0.25
done

SOCKET_PATH="$socket" SMOKE_PROMPT="$prompt" python3 - <<'PY'
import json
import os
import socket
import sys

socket_path = os.environ["SOCKET_PATH"]
prompt = os.environ["SMOKE_PROMPT"]

def request(sock, request_id, kind, payload):
    line = json.dumps({
        "schema": "agentlibre.daemon.request.v1alpha",
        "request_id": request_id,
        "kind": kind,
        "payload": payload,
    }, separators=(",", ":"))
    sock.sendall(line.encode("utf-8") + b"\n")

def read_event(file):
    line = file.readline()
    if not line:
        raise RuntimeError("daemon closed socket")
    event = json.loads(line)
    if event.get("schema") != "agentlibre.daemon.event.v1alpha":
        raise RuntimeError(f"unexpected schema: {event}")
    if event.get("kind") == "error":
        raise RuntimeError(f"daemon error: {event['payload']}")
    return event

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
    sock.settimeout(180)
    sock.connect(socket_path)
    file = sock.makefile("r", encoding="utf-8")

    request(sock, "req-hello", "hello", {
        "client_name": "agl-daemon-live-smoke",
        "accepted_protocol_versions": ["v1alpha"],
    })
    hello = read_event(file)
    assert hello["kind"] == "hello", hello

    request(sock, "req-open", "session_open", {
        "new_session": True,
        "tool_mode": "read-only",
    })
    opened = read_event(file)
    assert opened["kind"] == "session_opened", opened
    session_id = opened["payload"]["session_id"]

    request(sock, "req-turn", "session_turn", {
        "session_id": session_id,
        "text": prompt,
        "idempotency_key": "agl-daemon-live-smoke-event",
    })
    assistant = []
    terminal = None
    while terminal is None:
        event = read_event(file)
        if event["kind"] == "assistant_message":
            assistant.append(event["payload"]["content"])
        elif event["kind"] == "turn_finished":
            terminal = event["payload"]["status"]
    if terminal != "answered":
        raise RuntimeError(f"turn did not answer: {terminal}")
    if not "".join(assistant).strip():
        raise RuntimeError("assistant response was empty")

    request(sock, "req-transcript", "session_transcript", {
        "session_id": session_id,
        "include_content": False,
    })
    transcript = read_event(file)
    assert transcript["kind"] == "session_transcript", transcript
    if transcript["payload"]["content_included"]:
        raise RuntimeError("safe transcript unexpectedly included content")
    if not transcript["payload"]["events"]:
        raise RuntimeError("safe transcript was empty")

    print(f"session_id={session_id}")
    print(f"assistant_bytes={len(''.join(assistant).encode('utf-8'))}")
    print(f"transcript_events={len(transcript['payload']['events'])}")
PY

echo "smoke_home=$home"
echo "daemon_log=$daemon_log"
echo "serve_mode=$serve_mode"
