#!/usr/bin/env python3
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

POLICY_SCHEMA = "agentlibre.architecture-boundary.v1"
DEPENDENCY_KINDS = ("normal", "build", "dev")
FUTURE_STATUSES = ("not-yet-present", "in-tree-split", "external-pinned")
FUTURE_KINDS = ("crate", "binary")
PINNED_GIT_REVISION = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})")
DEFAULT_POLICY = Path(__file__).parent / "architecture" / "agl151-terminal-boundary.json"


def load_json(path, label):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ValueError(f"{label} is unreadable or invalid JSON: {error}") from error


def load_metadata(path):
    if path:
        return load_json(path, "metadata")
    try:
        completed = subprocess.run(
            ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise ValueError(f"cargo metadata failed: {error}") from error
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise ValueError(f"cargo metadata returned invalid JSON: {error}") from error


def validate_inventory(policy):
    diagnostics = []
    if not isinstance(policy, dict):
        return {}, {}, ["policy: root must be an object"]
    if policy.get("schema") != POLICY_SCHEMA:
        diagnostics.append(f"policy: schema must equal {POLICY_SCHEMA}")
    if policy.get("exact_identity_matching") is not True:
        diagnostics.append("policy: exact_identity_matching must be true")
    if policy.get("dependency_kinds") != list(DEPENDENCY_KINDS):
        diagnostics.append("policy: dependency_kinds must equal normal, build, dev")
    inventory = policy.get("inventory")
    if not isinstance(inventory, dict):
        return {}, {}, diagnostics + ["policy: inventory must be an object"]

    parsed = {}
    for group in ("current", "future"):
        entries = inventory.get(group)
        if not isinstance(entries, list):
            diagnostics.append(f"policy: inventory.{group} must be a list")
            continue
        parsed[group] = {}
        for index, entry in enumerate(entries):
            if not isinstance(entry, dict):
                diagnostics.append(f"policy: inventory.{group}[{index}] must be an object")
                continue
            invalid = [
                field
                for field in ("name", "owner", "category")
                if not isinstance(entry.get(field), str) or not entry[field]
            ]
            if group == "future":
                if entry.get("status") not in FUTURE_STATUSES:
                    invalid.append("status")
                if entry.get("kind") not in FUTURE_KINDS:
                    invalid.append("kind")
            if invalid:
                diagnostics.append(
                    f"policy: inventory.{group}[{index}] invalid fields: {','.join(invalid)}"
                )
                continue
            name = entry["name"]
            if name in parsed[group]:
                diagnostics.append(f"policy: duplicate {group} identity: {name}")
            parsed[group][name] = entry

    current = parsed.get("current", {})
    future = parsed.get("future", {})
    for name in sorted(current.keys() & future.keys()):
        diagnostics.append(f"policy: identity is both current and future: {name}")
    return current, future, diagnostics


def validate_metadata(metadata):
    if not isinstance(metadata, dict) or not isinstance(metadata.get("packages"), list):
        return {}, ["metadata: packages must be a list"]
    packages = {}
    diagnostics = []
    for index, package in enumerate(metadata["packages"]):
        if not isinstance(package, dict):
            diagnostics.append(f"metadata: packages[{index}] must be an object")
            continue
        name = package.get("name")
        dependencies = package.get("dependencies")
        if not isinstance(name, str) or not name:
            diagnostics.append(f"metadata: packages[{index}].name must be a string")
            continue
        if not isinstance(dependencies, list):
            diagnostics.append(f"metadata: {name}.dependencies must be a list")
            continue
        if name in packages:
            diagnostics.append(f"metadata: duplicate package identity: {name}")
        packages[name] = package
    return packages, diagnostics


def dependency_diagnostics(packages, current, future):
    diagnostics = set()
    identities = current | future
    future_names = set(future)
    for name, package in packages.items():
        info = identities.get(name)
        if info is None:
            continue
        if name in future and info["category"] == "terminal":
            source = package.get("source")
            if info["status"] == "in-tree-split":
                if source is not None:
                    diagnostics.add(
                        f"source: {name} in-tree-split requires a workspace path source"
                    )
            else:
                revision = (
                    source.rsplit("#", 1)[-1] if isinstance(source, str) else ""
                )
                if (
                    not isinstance(source, str)
                    or not source.startswith("git+")
                    or "#" not in source
                    or PINNED_GIT_REVISION.fullmatch(revision) is None
                ):
                    diagnostics.add(
                        f"source: {name} requires an immutable pinned git revision"
                    )

        for index, dependency in enumerate(package["dependencies"]):
            if not isinstance(dependency, dict):
                diagnostics.add(f"metadata: {name}.dependencies[{index}] must be an object")
                continue
            target = dependency.get("name")
            kind = dependency.get("kind") or "normal"
            if not isinstance(target, str) or not target:
                diagnostics.add(f"metadata: {name}.dependencies[{index}].name must be a string")
                continue
            if kind not in DEPENDENCY_KINDS:
                diagnostics.add(f"metadata: {name} -> {target} has unknown kind {kind!r}")
                continue
            target_info = identities.get(target)
            if target_info is None:
                continue
            category = info["category"]
            if category == "terminal" and name != "agl-terminal-ui":
                if target_info["owner"] == "agentLIBRE":
                    diagnostics.add(f"{kind}: {name} -> {target} crosses terminal-to-agent")
            if name == "agl-terminal-ui":
                if target_info["owner"] == "agentLIBRE" and target not in {
                    "agl-client",
                    "agl-content",
                    "agl-ids",
                    "agl-protocol",
                }:
                    diagnostics.add(f"{kind}: {name} -> {target} exceeds the bounded agent SDK")
            if category == "terminal" and name != "agl-terminal-ui" and (
                target in {"agl-protocol", "agl-store"}
                or target_info["category"] in {"presentation", "inference"}
            ):
                diagnostics.add(f"{kind}: {name} -> {target} is forbidden below terminal UI")
            if category == "inference" and target in future_names:
                diagnostics.add(f"{kind}: {name} -> {target} crosses inference-to-terminal")
    return diagnostics


def run(policy_path, metadata_path):
    policy = load_json(policy_path, "policy")
    metadata = load_metadata(metadata_path)
    current, future, diagnostics = validate_inventory(policy)
    packages, metadata_errors = validate_metadata(metadata)
    diagnostics.extend(metadata_errors)
    package_names = set(packages)
    diagnostics.extend(
        f"inventory: missing current package: {name}"
        for name in sorted(set(current) - package_names)
    )
    diagnostics.extend(
        f"inventory: undeclared metadata package: {name}"
        for name in sorted(package_names - (set(current) | set(future)))
    )
    diagnostics.extend(dependency_diagnostics(packages, current, future))
    if diagnostics:
        for diagnostic in sorted(set(diagnostics)):
            print(diagnostic, file=sys.stderr)
        return 1
    present_future = len(package_names & set(future))
    print(
        f"architecture-boundary: ok current={len(current)} "
        f"future_declared={len(future)} future_present={present_future}"
    )
    return 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--policy", default=DEFAULT_POLICY)
    parser.add_argument("--metadata")
    arguments = parser.parse_args()
    try:
        return run(arguments.policy, arguments.metadata)
    except ValueError as error:
        print(f"architecture-boundary: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
