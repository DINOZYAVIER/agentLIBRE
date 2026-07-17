#!/usr/bin/env python3
"""Validate the privacy-safe AGL-139 native manager smoke summary."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path
from typing import Any, NoReturn


DEFAULT_SUMMARY = Path(".agl/smoke/AGL-139/native-manager.json")
ID_PATTERNS = {
    "run_id": re.compile(r"^run_[0-9a-f-]{36}$"),
    "turn_id": re.compile(r"^turn_[0-9a-f-]{36}$"),
    "attempt_id": re.compile(r"^attempt_[0-9a-f-]{36}$"),
}
DIGEST = re.compile(r"^[0-9a-f]{64}$")
CONFIG_DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
FORBIDDEN_KEY_PARTS = (
    "prompt",
    "content",
    "output",
    "model_path",
    "config_path",
    "native_log",
    "runtime_log",
)


def fail(message: str) -> NoReturn:
    raise SystemExit(f"AGL-139 smoke summary invalid: {message}")


def require_object(value: Any, name: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{name} must be an object")
    if set(value) != keys:
        fail(f"{name} fields differ: expected {sorted(keys)}, got {sorted(value)}")
    return value


def require_bool(value: Any, name: str, expected: bool = True) -> None:
    if value is not expected:
        fail(f"{name} must be {str(expected).lower()}")


def walk_safe(value: Any, key: str | None = None) -> None:
    if key is not None and any(part in key for part in FORBIDDEN_KEY_PARTS):
        fail(f"forbidden field {key!r}")
    if isinstance(value, dict):
        for child_key, child in value.items():
            walk_safe(child, child_key)
    elif isinstance(value, list):
        for child in value:
            walk_safe(child, key)
    elif isinstance(value, str):
        if value.startswith(("/", "file:")) or ".." in value:
            fail("absolute or parent-relative path found")


def validate_attempts(value: Any) -> tuple[dict[str, dict[str, Any]], set[str]]:
    if not isinstance(value, list) or len(value) != 5:
        fail("attempts must contain exactly five entries")
    attempts: dict[str, dict[str, Any]] = {}
    generation_attempts: set[str] = set()
    for index, raw in enumerate(value):
        required = {
            "label",
            "run_id",
            "turn_id",
            "attempt_id",
            "context_key_digest",
            "evidence_started",
        }
        if raw.get("evidence_started") is True:
            required.add("events_ref")
        attempt = require_object(raw, f"attempts[{index}]", required)
        label = attempt["label"]
        if label in attempts or label not in {
            "warm_a",
            "warm_b",
            "active_cancel",
            "queued_cancel",
            "replacement",
        }:
            fail(f"unexpected or duplicate attempt label {label!r}")
        for field, pattern in ID_PATTERNS.items():
            if not isinstance(attempt[field], str) or pattern.fullmatch(attempt[field]) is None:
                fail(f"{label}.{field} is not a canonical typed ID")
        if DIGEST.fullmatch(attempt["context_key_digest"]) is None:
            fail(f"{label}.context_key_digest is invalid")
        started = label != "queued_cancel"
        require_bool(attempt["evidence_started"], f"{label}.evidence_started", started)
        if started:
            expected_ref = f"runs/{attempt['run_id']}/events.jsonl"
            if attempt["events_ref"] != expected_ref:
                fail(f"{label}.events_ref does not match its run")
            generation_attempts.add(attempt["attempt_id"])
        attempts[label] = attempt
    return attempts, generation_attempts


def validate(path: Path) -> None:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(str(error))
    walk_safe(document)
    root = require_object(
        document,
        "root",
        {
            "schema",
            "task_id",
            "outcome",
            "model_key_digest",
            "config_digest",
            "context_key_digests",
            "attempts",
            "counters",
            "admission",
            "native_abort",
            "runtime_observations",
            "resource_lifecycle",
        },
    )
    if root["schema"] != "agentlibre.smoke.agl139.v1":
        fail("unexpected schema")
    if root["task_id"] != "AGL-139" or root["outcome"] != "passed":
        fail("task or outcome is not the required passing AGL-139 result")
    if DIGEST.fullmatch(root["model_key_digest"]) is None:
        fail("model_key_digest is invalid")
    if CONFIG_DIGEST.fullmatch(root["config_digest"]) is None:
        fail("config_digest is invalid")
    contexts = root["context_key_digests"]
    if (
        not isinstance(contexts, list)
        or len(contexts) != 2
        or len(set(contexts)) != 2
        or any(DIGEST.fullmatch(value) is None for value in contexts)
    ):
        fail("exactly two distinct context digests are required")

    attempts, expected_generation_attempts = validate_attempts(root["attempts"])

    counters = require_object(
        root["counters"],
        "counters",
        {
            "model_loads",
            "context_loads",
            "cached_contexts_before_shutdown",
            "completed_jobs",
            "cancellations",
            "deadline_exceeded",
            "failures",
        },
    )
    expected_counters = {
        "model_loads": 1,
        "context_loads": 2,
        "cached_contexts_before_shutdown": 1,
        "completed_jobs": 3,
        "cancellations": 2,
        "deadline_exceeded": 0,
        "failures": 0,
    }
    if counters != expected_counters:
        fail(f"counter evidence differs: {counters!r}")

    admission = require_object(
        root["admission"],
        "admission",
        {
            "queue_capacity",
            "queued_depth_observed",
            "depth_after_queued_cancel",
            "replacement_depth_before_active_release",
            "queued_cancel_reclaimed_capacity",
            "replacement_admitted_while_active",
            "queued_attempt_never_started",
            "queued_attempt_has_no_evidence",
            "replacement_succeeded",
        },
    )
    if [
        admission["queue_capacity"],
        admission["queued_depth_observed"],
        admission["depth_after_queued_cancel"],
        admission["replacement_depth_before_active_release"],
    ] != [1, 1, 0, 1]:
        fail("queue depth evidence is not 1 -> 0 -> 1")
    for field in (
        "queued_cancel_reclaimed_capacity",
        "replacement_admitted_while_active",
        "queued_attempt_never_started",
        "queued_attempt_has_no_evidence",
        "replacement_succeeded",
    ):
        require_bool(admission[field], f"admission.{field}")

    native_abort = require_object(
        root["native_abort"],
        "native_abort",
        {
            "callback_installations",
            "callback_calls",
            "aborting_callback_calls",
            "install_wait_timed_out",
            "active_attempt_cancelled",
        },
    )
    if native_abort["callback_installations"] != 1:
        fail("native abort callback must be installed exactly once")
    if native_abort["callback_calls"] < 1 or native_abort["aborting_callback_calls"] < 1:
        fail("native abort callback invocation evidence is missing")
    require_bool(native_abort["install_wait_timed_out"], "native_abort.install_wait_timed_out", False)
    require_bool(native_abort["active_attempt_cancelled"], "native_abort.active_attempt_cancelled")

    runtime = require_object(
        root["runtime_observations"],
        "runtime_observations",
        {
            "model_load_digests",
            "context_create_digests",
            "generation_attempt_ids",
            "rendered_message_counts_by_context",
        },
    )
    if runtime["model_load_digests"] != [root["model_key_digest"]]:
        fail("runtime model-load evidence differs from the model digest")
    if set(runtime["context_create_digests"]) != set(contexts) or len(runtime["context_create_digests"]) != 2:
        fail("runtime context-create evidence differs from the context digests")
    if set(runtime["generation_attempt_ids"]) != expected_generation_attempts:
        fail("native generation attempts differ from started attempt evidence")
    if attempts["queued_cancel"]["attempt_id"] in runtime["generation_attempt_ids"]:
        fail("queued cancelled attempt reached native generation")
    histories = runtime["rendered_message_counts_by_context"]
    if set(histories) != set(contexts) or sorted(histories.values()) != [[1, 3], [1, 3]]:
        fail("two independent 1 -> 3 message histories were not observed")

    lifecycle = require_object(
        root["resource_lifecycle"],
        "resource_lifecycle",
        {"drops", "all_contexts_dropped_before_model"},
    )
    require_bool(
        lifecycle["all_contexts_dropped_before_model"],
        "resource_lifecycle.all_contexts_dropped_before_model",
    )
    drops = lifecycle["drops"]
    if not isinstance(drops, list) or len(drops) != 3:
        fail("resource drop evidence must contain two contexts and one model")
    for index, drop in enumerate(drops):
        require_object(drop, f"resource_lifecycle.drops[{index}]", {"kind", "digest"})
    if [drop["kind"] for drop in drops] != ["context", "context", "model"]:
        fail("resource drop order is not context, context, model")
    if {drop["digest"] for drop in drops[:2]} != set(contexts):
        fail("context drop digests differ from created contexts")
    if drops[2]["digest"] != root["model_key_digest"]:
        fail("model drop digest differs from the loaded model")


def main() -> None:
    if len(sys.argv) > 2:
        fail("usage: scripts/validate-agl139-smoke.py [summary.json]")
    path = Path(sys.argv[1]) if len(sys.argv) == 2 else DEFAULT_SUMMARY
    validate(path)
    print("AGL-139 smoke summary: valid")


if __name__ == "__main__":
    main()
