#!/usr/bin/env bash

fail() {
  echo "smoke failed: $*" >&2
  exit 1
}

need_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"
}

new_typed_id() {
  local kind="$1"
  local prefix
  case "$kind" in
    session) prefix="ses" ;;
    run) prefix="run" ;;
    turn) prefix="turn" ;;
    step) prefix="step" ;;
    attempt) prefix="attempt" ;;
    event) prefix="evt" ;;
    request) prefix="req" ;;
    message) prefix="msg" ;;
    *) fail "unsupported typed ID kind: $kind" ;;
  esac
  python3 - "$prefix" <<'PY'
import os
import sys
import time
import uuid

unix_ms = time.time_ns() // 1_000_000
random_bits = int.from_bytes(os.urandom(10), "big")
value = (
    (unix_ms << 80)
    | (0x7 << 76)
    | (((random_bits >> 62) & 0xFFF) << 64)
    | (0b10 << 62)
    | (random_bits & ((1 << 62) - 1))
)
print(f"{sys.argv[1]}_{uuid.UUID(int=value)}")
PY
}

require_file() {
  [[ -f "$1" ]] || fail "missing file: $1"
}

require_contains() {
  local path="$1"
  local needle="$2"
  require_file "$path"
  grep -F -- "$needle" "$path" >/dev/null || fail "$path does not contain: $needle"
}

require_not_contains() {
  local path="$1"
  local needle="$2"
  require_file "$path"
  if grep -F -- "$needle" "$path" >/dev/null; then
    fail "$path unexpectedly contains: $needle"
  fi
}

smoke_abs_path() {
  local path="$1"
  local dir
  dir="$(cd -- "$(dirname -- "$path")" && pwd)"
  printf '%s/%s' "$dir" "$(basename -- "$path")"
}

json_content() {
  python3 - "$1" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    print(json.load(handle)["content"], end="")
PY
}

json_metadata_value() {
  python3 - "$1" "$2" <<'PY'
import json
import sys

path, key = sys.argv[1:]
with open(path, encoding="utf-8") as handle:
    metadata = json.load(handle).get("metadata")
if not isinstance(metadata, dict) or key not in metadata:
    raise SystemExit(f"{path}: response metadata has no {key!r}")
value = metadata[key]
if isinstance(value, (dict, list)):
    raise SystemExit(f"{path}: response metadata {key!r} is not scalar")
print(value, end="")
PY
}

require_json_metadata_value() {
  local path="$1"
  local key="$2"
  local expected="$3"
  local actual
  require_file "$path"
  actual="$(json_metadata_value "$path" "$key")"
  [[ "$actual" == "$expected" ]] ||
    fail "$path metadata $key is $actual, expected $expected"
}

runtime_attempt_id() {
  local events_path="$1"
  local ordinal="$2"
  python3 - "$events_path" "$ordinal" <<'PY'
import json
import sys
import uuid

path = sys.argv[1]
ordinal = int(sys.argv[2])
attempt_ids = []
with open(path, encoding="utf-8") as handle:
    for line in handle:
        if not line.strip():
            continue
        event = json.loads(line)
        if event.get("schema") != "agentlibre.event.v1alpha":
            raise SystemExit(f"{path}: unexpected runtime event schema")
        payload = event.get("payload", {})
        scope = event.get("scope", {})
        if not isinstance(payload, dict) or not isinstance(scope, dict):
            raise SystemExit(f"{path}: runtime event has invalid scope or payload")
        if payload.get("kind") != "inference.attempt_started":
            continue
        attempt_id = scope.get("attempt_id")
        if attempt_id and attempt_id not in attempt_ids:
            if not attempt_id.startswith("attempt_"):
                raise SystemExit(f"{path}: invalid attempt ID {attempt_id!r}")
            payload = attempt_id.removeprefix("attempt_")
            try:
                parsed = uuid.UUID(payload)
            except ValueError as error:
                raise SystemExit(f"{path}: invalid attempt ID {attempt_id!r}") from error
            if str(parsed) != payload or parsed.version != 7:
                raise SystemExit(
                    f"{path}: non-canonical UUIDv7 attempt ID {attempt_id!r}"
                )
            attempt_ids.append(attempt_id)
try:
    print(attempt_ids[ordinal - 1])
except IndexError as error:
    raise SystemExit(
        f"{path}: missing runtime attempt {ordinal}; found {attempt_ids!r}"
    ) from error
PY
}

single_run_id() {
  local artifact_root="$1"
  python3 - "$artifact_root" <<'PY'
import pathlib
import sys
import uuid

runs_root = pathlib.Path(sys.argv[1]) / "runs"
run_ids = sorted(path.name for path in runs_root.glob("run_*") if path.is_dir())
if len(run_ids) != 1:
    raise SystemExit(f"{runs_root}: expected one run directory, found {run_ids!r}")
payload = run_ids[0].removeprefix("run_")
try:
    parsed = uuid.UUID(payload)
except ValueError as error:
    raise SystemExit(f"{runs_root}: invalid run ID {run_ids[0]!r}") from error
if str(parsed) != payload or parsed.version != 7:
    raise SystemExit(f"{runs_root}: non-canonical UUIDv7 run ID {run_ids[0]!r}")
print(run_ids[0])
PY
}

normalize_runtime_events() {
  local events_path="$1"
  local normalized_path="$2"
  python3 - "$events_path" "$normalized_path" <<'PY'
import json
import sys
import uuid


def require_id(value, prefix, context):
    if not isinstance(value, str) or not value.startswith(f"{prefix}_"):
        raise SystemExit(f"{context}: expected {prefix}_ ID, got {value!r}")
    payload = value[len(prefix) + 1 :]
    try:
        parsed = uuid.UUID(payload)
    except ValueError as error:
        raise SystemExit(f"{context}: invalid {prefix}_ ID {value!r}") from error
    if str(parsed) != payload or parsed.version != 7:
        raise SystemExit(f"{context}: non-canonical UUIDv7 {prefix}_ ID {value!r}")


source_path, output_path = sys.argv[1:]
required_fields = {
    "schema",
    "event_id",
    "sequence",
    "occurred_at_unix_ms",
    "scope",
    "payload",
}
optional_fields = {"request_id", "caused_by"}
normalized = []
event_ids = set()
run_sequences = {}
with open(source_path, encoding="utf-8") as handle:
    for line_number, line in enumerate(handle, 1):
        context = f"{source_path}:{line_number}"
        if not line.strip():
            raise SystemExit(f"{context}: empty runtime event record")
        envelope = json.loads(line)
        fields = set(envelope)
        if not required_fields.issubset(fields) or fields - required_fields - optional_fields:
            raise SystemExit(f"{context}: invalid runtime envelope fields {sorted(fields)!r}")
        if envelope.get("schema") != "agentlibre.event.v1alpha":
            raise SystemExit(f"{context}: unexpected event schema {envelope.get('schema')!r}")
        event_id = envelope.get("event_id")
        require_id(event_id, "evt", context)
        if event_id in event_ids:
            raise SystemExit(f"{context}: duplicate event ID {event_id}")
        event_ids.add(event_id)
        sequence = envelope.get("sequence")
        if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= 0:
            raise SystemExit(f"{context}: invalid event sequence {sequence!r}")
        occurred_at = envelope.get("occurred_at_unix_ms")
        if not isinstance(occurred_at, int) or isinstance(occurred_at, bool) or occurred_at <= 0:
            raise SystemExit(f"{context}: invalid event timestamp {occurred_at!r}")

        scope = envelope.get("scope")
        payload = envelope.get("payload")
        if not isinstance(scope, dict) or not isinstance(payload, dict):
            raise SystemExit(f"{context}: runtime envelope has invalid scope or payload")
        if set(scope) - {"run_id", "session_id", "turn_id", "step_id", "attempt_id"}:
            raise SystemExit(f"{context}: unexpected scope fields {sorted(scope)!r}")
        run_id = scope.get("run_id")
        require_id(run_id, "run", context)
        for field, prefix in (
            ("session_id", "ses"),
            ("turn_id", "turn"),
            ("step_id", "step"),
            ("attempt_id", "attempt"),
        ):
            if field in scope:
                require_id(scope[field], prefix, context)
        expected_sequence = run_sequences.get(run_id, 0) + 1
        if sequence != expected_sequence:
            raise SystemExit(
                f"{context}: run {run_id} sequence {sequence}, expected {expected_sequence}"
            )
        run_sequences[run_id] = sequence
        if not isinstance(payload.get("kind"), str):
            raise SystemExit(f"{context}: runtime event payload has no kind")
        overlap = set(scope).intersection(payload)
        if overlap:
            raise SystemExit(f"{context}: scope/payload fields overlap: {sorted(overlap)!r}")

        logical = dict(scope)
        logical.update(payload)
        logical["_event_id"] = event_id
        logical["_sequence"] = sequence
        logical["_occurred_at_unix_ms"] = occurred_at
        if "request_id" in envelope:
            require_id(envelope["request_id"], "req", context)
            logical["_request_id"] = envelope["request_id"]
        if "caused_by" in envelope:
            require_id(envelope["caused_by"], "evt", context)
            logical["_caused_by"] = envelope["caused_by"]
        normalized.append(logical)

with open(output_path, "w", encoding="utf-8") as output:
    for event in normalized:
        json.dump(event, output, separators=(",", ":"), sort_keys=True)
        output.write("\n")
PY
}

normalize_transcript() {
  local transcript_path="$1"
  local normalized_path="$2"
  python3 - "$transcript_path" "$normalized_path" <<'PY'
import json
import sys
import uuid


def require_id(value, prefix, context):
    if not isinstance(value, str) or not value.startswith(f"{prefix}_"):
        raise SystemExit(f"{context}: expected {prefix}_ ID, got {value!r}")
    payload = value[len(prefix) + 1 :]
    try:
        parsed = uuid.UUID(payload)
    except ValueError as error:
        raise SystemExit(f"{context}: invalid {prefix}_ ID {value!r}") from error
    if str(parsed) != payload or parsed.version != 7:
        raise SystemExit(f"{context}: non-canonical UUIDv7 {prefix}_ ID {value!r}")


source_path, output_path = sys.argv[1:]
lifecycle_kinds = {
    "session_started",
    "context_cleared",
    "session_finished",
    "session_failed",
}
runtime_kinds = {
    "user_message",
    "assistant_message",
    "assistant_tool_call",
    "tool_message",
    "model_attempt_linked",
}
required_envelope_fields = {
    "schema",
    "event_id",
    "sequence",
    "occurred_at_unix_ms",
    "scope",
    "payload",
}
optional_envelope_fields = {"request_id", "caused_by"}
payload_fields = {
    "user_message": {"kind", "message_id", "content"},
    "assistant_message": {"kind", "message_id", "content"},
    "assistant_tool_call": {"kind", "message_id", "name", "arguments"},
    "tool_message": {"kind", "message_id", "name", "content"},
    "model_attempt_linked": {"kind"},
}
lifecycle_fields = {
    "session_started": {"kind", "session_id"},
    "context_cleared": {"kind", "session_id"},
    "session_finished": {"kind", "session_id", "reason"},
    "session_failed": {"kind", "session_id", "message"},
}
normalized = []
session_ids = set()
event_ids = set()
run_sequences = {}
with open(source_path, encoding="utf-8") as handle:
    for line_number, line in enumerate(handle, 1):
        if not line.strip():
            raise SystemExit(f"{source_path}:{line_number}: empty transcript record")
        record = json.loads(line)
        record_kind = record.get("kind")
        context = f"{source_path}:{line_number}"
        if record_kind in lifecycle_kinds:
            if set(record) != lifecycle_fields[record_kind]:
                raise SystemExit(f"{context}: invalid {record_kind} lifecycle fields")
            session_id = record.get("session_id")
            require_id(session_id, "ses", context)
            session_ids.add(session_id)
            normalized.append(record)
            continue
        if record_kind != "runtime" or set(record) != {"kind", "envelope"}:
            raise SystemExit(f"{context}: unexpected transcript record {record!r}")

        envelope = record.get("envelope")
        if not isinstance(envelope, dict):
            raise SystemExit(f"{context}: runtime record has no event envelope")
        envelope_fields = set(envelope)
        if (
            not required_envelope_fields.issubset(envelope_fields)
            or envelope_fields - required_envelope_fields - optional_envelope_fields
        ):
            raise SystemExit(
                f"{context}: invalid runtime envelope fields {sorted(envelope_fields)!r}"
            )
        if envelope.get("schema") != "agentlibre.event.v1alpha":
            raise SystemExit(f"{context}: unexpected event schema {envelope.get('schema')!r}")
        event_id = envelope.get("event_id")
        require_id(event_id, "evt", context)
        if event_id in event_ids:
            raise SystemExit(f"{context}: duplicate event ID {event_id}")
        event_ids.add(event_id)
        sequence = envelope.get("sequence")
        if not isinstance(sequence, int) or isinstance(sequence, bool) or sequence <= 0:
            raise SystemExit(f"{context}: invalid event sequence {sequence!r}")
        occurred_at = envelope.get("occurred_at_unix_ms")
        if not isinstance(occurred_at, int) or isinstance(occurred_at, bool) or occurred_at <= 0:
            raise SystemExit(f"{context}: invalid event timestamp {occurred_at!r}")

        scope = envelope.get("scope")
        payload = envelope.get("payload")
        if not isinstance(scope, dict) or not isinstance(payload, dict):
            raise SystemExit(f"{context}: runtime envelope has invalid scope or payload")
        if set(scope) - {"run_id", "session_id", "turn_id", "attempt_id"}:
            raise SystemExit(f"{context}: unexpected transcript scope fields {sorted(scope)!r}")
        run_id = scope.get("run_id")
        session_id = scope.get("session_id")
        turn_id = scope.get("turn_id")
        require_id(run_id, "run", context)
        require_id(session_id, "ses", context)
        require_id(turn_id, "turn", context)
        session_ids.add(session_id)
        previous_sequence = run_sequences.get(run_id)
        if previous_sequence is not None and sequence <= previous_sequence:
            raise SystemExit(
                f"{context}: run {run_id} sequence {sequence} is not greater than {previous_sequence}"
            )
        run_sequences[run_id] = sequence

        payload_kind = payload.get("kind")
        if payload_kind not in runtime_kinds:
            raise SystemExit(f"{context}: unexpected transcript runtime kind {payload_kind!r}")
        if set(payload) != payload_fields[payload_kind]:
            raise SystemExit(f"{context}: invalid {payload_kind} payload fields")
        attempt_id = scope.get("attempt_id")
        if payload_kind == "model_attempt_linked":
            require_id(attempt_id, "attempt", context)
        elif attempt_id is not None:
            raise SystemExit(f"{context}: {payload_kind} unexpectedly carries attempt scope")
        if "message_id" in payload:
            require_id(payload["message_id"], "msg", context)
        overlap = set(scope).intersection(payload)
        if overlap:
            raise SystemExit(f"{context}: scope/payload fields overlap: {sorted(overlap)!r}")

        logical = dict(scope)
        logical.update(payload)
        logical["_event_id"] = event_id
        logical["_sequence"] = sequence
        logical["_occurred_at_unix_ms"] = occurred_at
        if "request_id" in envelope:
            request_id = envelope["request_id"]
            require_id(request_id, "req", context)
            logical["_request_id"] = request_id
        if "caused_by" in envelope:
            caused_by = envelope["caused_by"]
            require_id(caused_by, "evt", context)
            logical["_caused_by"] = caused_by
        normalized.append(logical)

if len(session_ids) != 1:
    raise SystemExit(f"{source_path}: transcript session IDs disagree: {sorted(session_ids)!r}")
with open(output_path, "w", encoding="utf-8") as output:
    for record in normalized:
        json.dump(record, output, separators=(",", ":"), sort_keys=True)
        output.write("\n")
PY
}

transcript_turn_ids() {
  local normalized_path="$1"
  python3 - "$normalized_path" <<'PY'
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


turns = []
with open(sys.argv[1], encoding="utf-8") as handle:
    for line in handle:
        if not line.strip():
            continue
        event = json.loads(line)
        if event.get("kind") != "user_message":
            continue
        run_id = event.get("run_id")
        turn_id = event.get("turn_id")
        require_id(run_id, "run")
        require_id(turn_id, "turn")
        turns.append((run_id, turn_id))
if not turns:
    raise SystemExit(f"{sys.argv[1]}: transcript has no submitted turns")
for run_id, turn_id in turns:
    print(f"{run_id}\t{turn_id}")
PY
}
