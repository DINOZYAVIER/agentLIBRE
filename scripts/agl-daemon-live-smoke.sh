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
import time
import uuid

socket_path = os.environ["SOCKET_PATH"]
prompt = os.environ["SMOKE_PROMPT"]


def generate_id(prefix):
    unix_ms = time.time_ns() // 1_000_000
    random_bits = int.from_bytes(os.urandom(10), "big")
    value = (
        (unix_ms << 80)
        | (0x7 << 76)
        | (((random_bits >> 62) & 0xFFF) << 64)
        | (0b10 << 62)
        | (random_bits & ((1 << 62) - 1))
    )
    return f"{prefix}_{uuid.UUID(int=value)}"


def require_id(value, prefix):
    if not isinstance(value, str) or not value.startswith(f"{prefix}_"):
        raise RuntimeError(f"expected {prefix}_ ID, got {value!r}")
    payload = value[len(prefix) + 1 :]
    try:
        parsed = uuid.UUID(payload)
    except (ValueError, AttributeError) as error:
        raise RuntimeError(f"invalid {prefix}_ ID: {value!r}") from error
    if str(parsed) != payload or parsed.version != 7:
        raise RuntimeError(f"non-canonical UUIDv7 {prefix}_ ID: {value!r}")
    return value


def request(sock, kind, payload):
    request_id = generate_id("req")
    line = json.dumps({
        "schema": "agentlibre.daemon.request.v2alpha",
        "request_id": request_id,
        "kind": kind,
        "payload": payload,
    }, separators=(",", ":"))
    sock.sendall(line.encode("utf-8") + b"\n")
    return request_id


def read_event(file, request_id):
    line = file.readline()
    if not line:
        raise RuntimeError("daemon closed socket")
    event = json.loads(line)
    if event.get("schema") != "agentlibre.daemon.event.v2alpha":
        raise RuntimeError(f"unexpected schema: {event}")
    if event.get("request_id") != request_id:
        raise RuntimeError(
            f"response request mismatch: expected {request_id!r}, got {event!r}"
        )
    if event.get("kind") == "error":
        raise RuntimeError(f"daemon error: {event['payload']}")
    return event


def require_turn_identity(payload, session_id, run_id, turn_id):
    expected = {
        "session_id": session_id,
        "run_id": run_id,
        "turn_id": turn_id,
    }
    actual = {key: payload.get(key) for key in expected}
    if actual != expected:
        raise RuntimeError(f"turn identity mismatch: expected {expected!r}, got {payload!r}")


def require_runtime_envelope(envelope, request_id, session_id, run_id, turn_id):
    if envelope.get("schema") != "agentlibre.event.v1alpha":
        raise RuntimeError(f"unexpected runtime event schema: {envelope!r}")
    require_id(envelope.get("event_id"), "evt")
    sequence = envelope.get("sequence")
    if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= 0:
        raise RuntimeError(f"invalid runtime event sequence: {envelope!r}")
    occurred_at = envelope.get("occurred_at_unix_ms")
    if not isinstance(occurred_at, int) or isinstance(occurred_at, bool) or occurred_at <= 0:
        raise RuntimeError(f"invalid runtime event timestamp: {envelope!r}")
    if envelope.get("request_id") != request_id:
        raise RuntimeError(f"runtime event request mismatch: {envelope!r}")
    scope = envelope.get("scope", {})
    if scope.get("run_id") != run_id or scope.get("turn_id") != turn_id:
        raise RuntimeError(f"runtime event turn scope mismatch: {envelope!r}")
    if scope.get("session_id") != session_id:
        raise RuntimeError(f"runtime event session scope mismatch: {envelope!r}")
    payload = envelope.get("payload")
    if not isinstance(payload, dict) or not isinstance(payload.get("kind"), str):
        raise RuntimeError(f"runtime event has no typed payload: {envelope!r}")
    return sequence, payload["kind"], envelope.get("request_id")


def run_turn(sock, file, session_id, text, idempotency_key):
    request_id = request(sock, "session_turn", {
        "session_id": session_id,
        "text": text,
        "idempotency_key": idempotency_key,
    })
    started = read_event(file, request_id)
    if started.get("kind") != "turn_started":
        raise RuntimeError(f"expected turn_started, got {started!r}")
    run_id = require_id(started["payload"].get("run_id"), "run")
    turn_id = require_id(started["payload"].get("turn_id"), "turn")
    require_turn_identity(started["payload"], session_id, run_id, turn_id)

    assistant = []
    runtime_sequences = []
    runtime_kinds = []
    runtime_request_ids = []
    terminal = None
    failure = None
    while terminal is None:
        event = read_event(file, request_id)
        kind = event.get("kind")
        payload = event.get("payload", {})
        if kind == "runtime_event":
            sequence, runtime_kind, runtime_request_id = require_runtime_envelope(
                payload, request_id, session_id, run_id, turn_id
            )
            runtime_sequences.append(sequence)
            runtime_kinds.append(runtime_kind)
            runtime_request_ids.append(runtime_request_id)
        elif kind == "assistant_message":
            require_turn_identity(payload, session_id, run_id, turn_id)
            assistant.append(payload["content"])
        elif kind == "turn_stopped":
            require_turn_identity(payload, session_id, run_id, turn_id)
            failure = payload.get("reason")
        elif kind == "turn_failed":
            require_turn_identity(payload, session_id, run_id, turn_id)
            failure = payload.get("message")
        elif kind == "turn_finished":
            require_turn_identity(payload, session_id, run_id, turn_id)
            terminal = payload["status"]
        else:
            raise RuntimeError(f"unexpected turn event: {event!r}")

    if terminal != "answered":
        raise RuntimeError(f"turn did not answer: {terminal}: {failure}")
    answer = "".join(assistant)
    if not answer.strip():
        raise RuntimeError("assistant response was empty")
    if not runtime_sequences:
        raise RuntimeError("turn emitted no runtime_event envelopes")
    if runtime_sequences != list(range(1, len(runtime_sequences) + 1)):
        raise RuntimeError(f"runtime event sequence was not contiguous: {runtime_sequences!r}")
    for required_kind in ("turn.started", "assistant_message", "turn.finished"):
        if required_kind not in runtime_kinds:
            raise RuntimeError(
                f"runtime event stream omitted {required_kind!r}: {runtime_kinds!r}"
            )
    if request_id not in runtime_request_ids:
        raise RuntimeError("runtime event stream omitted request correlation")
    return {
        "answer": answer,
        "run_id": run_id,
        "turn_id": turn_id,
        "runtime_event_count": len(runtime_sequences),
    }


def require_transcript_ids(events, turns):
    correlated_kinds = {
        "user_message",
        "assistant_message",
        "assistant_tool_call",
        "tool_message",
        "model_attempt_linked",
    }
    expected_turns = {(turn["run_id"], turn["turn_id"]) for turn in turns}
    actual_turns = set()
    for event in events:
        kind = event.get("kind")
        if kind not in correlated_kinds:
            continue
        run_id = require_id(event.get("run_id"), "run")
        turn_id = require_id(event.get("turn_id"), "turn")
        actual_turns.add((run_id, turn_id))
        if "message_id" in event:
            require_id(event["message_id"], "msg")
        if "attempt_id" in event:
            require_id(event["attempt_id"], "attempt")
    if actual_turns != expected_turns:
        raise RuntimeError(
            f"transcript turn identities {actual_turns!r}, expected {expected_turns!r}"
        )


with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
    sock.settimeout(180)
    sock.connect(socket_path)
    file = sock.makefile("r", encoding="utf-8")

    hello_request_id = request(sock, "hello", {
        "client_name": "agl-daemon-live-smoke",
        "accepted_protocol_versions": ["v2alpha"],
    })
    hello = read_event(file, hello_request_id)
    assert hello["kind"] == "hello", hello
    assert hello["payload"]["protocol_version"] == "v2alpha", hello
    assert "runtime_events" in hello["payload"]["capabilities"], hello

    open_request_id = request(sock, "session_open", {
        "new_session": True,
        "tool_mode": "read-only",
    })
    opened = read_event(file, open_request_id)
    assert opened["kind"] == "session_opened", opened
    session_id = require_id(opened["payload"].get("session_id"), "ses")
    if opened["payload"].get("resumed") is not False:
        raise RuntimeError(f"new session was unexpectedly resumed: {opened!r}")

    turns = [
        run_turn(sock, file, session_id, prompt, "agl-daemon-live-smoke-turn-1"),
        run_turn(sock, file, session_id, prompt, "agl-daemon-live-smoke-turn-2"),
    ]
    if turns[0]["run_id"] == turns[1]["run_id"]:
        raise RuntimeError("two submitted turns reused one run ID")
    if turns[0]["turn_id"] == turns[1]["turn_id"]:
        raise RuntimeError("two submitted turns reused one turn ID")

    transcript_request_id = request(sock, "session_transcript", {
        "session_id": session_id,
        "include_content": False,
    })
    transcript = read_event(file, transcript_request_id)
    assert transcript["kind"] == "session_transcript", transcript
    assert transcript["payload"]["session_id"] == session_id, transcript
    if transcript["payload"]["content_included"]:
        raise RuntimeError("safe transcript unexpectedly included content")
    if not transcript["payload"]["events"]:
        raise RuntimeError("safe transcript was empty")
    require_transcript_ids(transcript["payload"]["events"], turns)

    print(f"session_id={session_id}")
    for index, turn in enumerate(turns, 1):
        print(f"turn_{index}_run_id={turn['run_id']}")
        print(f"turn_{index}_turn_id={turn['turn_id']}")
        print(f"turn_{index}_assistant_bytes={len(turn['answer'].encode('utf-8'))}")
        print(f"turn_{index}_runtime_events={turn['runtime_event_count']}")
    print(f"transcript_events={len(transcript['payload']['events'])}")
PY

echo "smoke_home=$home"
echo "daemon_log=$daemon_log"
echo "serve_mode=$serve_mode"
