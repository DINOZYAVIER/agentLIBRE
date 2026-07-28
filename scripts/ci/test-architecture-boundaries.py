#!/usr/bin/env python3
"""Exercise the AGL-151 architecture checker with deterministic metadata."""

import json
import subprocess
import sys
import tempfile
from pathlib import Path


CI_DIR = Path(__file__).resolve().parent
REPO_ROOT = CI_DIR.parents[1]
POLICY_PATH = CI_DIR / "architecture" / "agl151-terminal-boundary.json"
FIXTURES_PATH = (
    CI_DIR / "architecture" / "agl151-terminal-boundary-fixtures.json"
)
CHECKER_PATH = CI_DIR / "check-architecture-boundaries.py"
DOC_PATH = REPO_ROOT / "docs" / "architecture" / "agl151-terminal-boundary.md"


class FixtureError(Exception):
    """A deterministic fixture or assertion failure."""


def load_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise FixtureError(f"cannot read {path}: {error}") from error


def require_entry_list(policy, key):
    inventory = policy.get("inventory")
    if not isinstance(inventory, dict) or not isinstance(inventory.get(key), list):
        raise FixtureError(f"policy inventory.{key} must be an array")

    entries = inventory[key]
    names = []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict) or not isinstance(entry.get("name"), str):
            raise FixtureError(f"policy inventory.{key}[{index}] has no name")
        names.append(entry["name"])
    if len(names) != len(set(names)):
        raise FixtureError(f"policy inventory.{key} contains duplicate names")
    return entries


def validate_fixtures(fixtures, future_names):
    if not isinstance(fixtures, dict):
        raise FixtureError("fixture manifest must be an object")
    if fixtures.get("schema") != "agentlibre.architecture-boundary-fixtures.v1":
        raise FixtureError("fixture manifest has an unsupported schema")
    cases = fixtures.get("cases")
    if not isinstance(cases, list) or not cases:
        raise FixtureError("fixture manifest cases must be a non-empty array")

    required = {
        "name",
        "expected_exit",
        "expected_diagnostics",
        "add_packages",
        "dependencies",
    }
    seen = set()
    for index, case in enumerate(cases):
        if not isinstance(case, dict) or set(case) != required:
            raise FixtureError(f"fixture case {index} has invalid fields")
        name = case["name"]
        if not isinstance(name, str) or not name or name in seen:
            raise FixtureError(f"fixture case {index} has invalid or duplicate name")
        seen.add(name)
        if case["expected_exit"] not in (0, 1):
            raise FixtureError(f"fixture {name} has invalid expected_exit")
        if not isinstance(case["expected_diagnostics"], list) or not all(
            isinstance(item, str) and item
            for item in case["expected_diagnostics"]
        ):
            raise FixtureError(f"fixture {name} has invalid diagnostics")
        if not isinstance(case["add_packages"], list):
            raise FixtureError(f"fixture {name} add_packages must be an array")
        if not isinstance(case["dependencies"], list):
            raise FixtureError(f"fixture {name} dependencies must be an array")

        added = set()
        for package in case["add_packages"]:
            if not isinstance(package, dict) or set(package) != {"name", "source"}:
                raise FixtureError(f"fixture {name} has invalid added package")
            package_name = package["name"]
            if (
                not isinstance(package_name, str)
                or package_name not in future_names
                or package_name in added
            ):
                raise FixtureError(
                    f"fixture {name} has unknown or duplicate added package"
                )
            source = package["source"]
            if source is not None and not isinstance(source, str):
                raise FixtureError(f"fixture {name} has invalid package source")
            added.add(package_name)

        for dependency in case["dependencies"]:
            if not isinstance(dependency, dict) or set(dependency) != {
                "from",
                "name",
                "kind",
            }:
                raise FixtureError(f"fixture {name} has invalid dependency")
            if not all(
                isinstance(dependency[field], str) and dependency[field]
                for field in ("from", "name")
            ):
                raise FixtureError(f"fixture {name} has unnamed dependency")
            if dependency["kind"] not in (None, "normal", "build", "dev"):
                raise FixtureError(f"fixture {name} has invalid dependency kind")
    return cases


def validate_document(current_entries, future_entries):
    try:
        text = DOC_PATH.read_text(encoding="utf-8")
    except OSError as error:
        raise FixtureError(f"cannot read {DOC_PATH}: {error}") from error
    for entry in current_entries + future_entries:
        if entry["name"] not in text:
            raise FixtureError(
                f"architecture document does not mention {entry['name']}"
            )


def metadata_for(case, current_entries):
    packages = [
        {"name": entry["name"], "source": None, "dependencies": []}
        for entry in current_entries
    ]
    by_name = {package["name"]: package for package in packages}

    for added in case["add_packages"]:
        if added["name"] in by_name:
            raise FixtureError(
                f"fixture {case['name']} adds duplicate package {added['name']}"
            )
        package = {
            "name": added["name"],
            "source": added["source"],
            "dependencies": [],
        }
        packages.append(package)
        by_name[package["name"]] = package

    for dependency in case["dependencies"]:
        source = by_name.get(dependency["from"])
        if source is None:
            raise FixtureError(
                f"fixture {case['name']} dependency source "
                f"{dependency['from']} is absent"
            )
        source["dependencies"].append(
            {"name": dependency["name"], "kind": dependency["kind"]}
        )
    return {"packages": packages}


def run_case(case, current_entries, temporary_dir):
    metadata_path = temporary_dir / f"{case['name']}.json"
    metadata_path.write_text(
        json.dumps(metadata_for(case, current_entries), sort_keys=True),
        encoding="utf-8",
    )
    result = subprocess.run(
        [
            sys.executable,
            str(CHECKER_PATH),
            "--policy",
            str(POLICY_PATH),
            "--metadata",
            str(metadata_path),
        ],
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != case["expected_exit"]:
        raise FixtureError(
            f"fixture {case['name']} expected exit {case['expected_exit']}, "
            f"got {result.returncode}: {result.stderr.strip()}"
        )
    for diagnostic in case["expected_diagnostics"]:
        if diagnostic not in result.stderr:
            raise FixtureError(
                f"fixture {case['name']} missing diagnostic: {diagnostic}"
            )
    if result.returncode == 0 and "architecture-boundary: ok" not in result.stdout:
        raise FixtureError(f"fixture {case['name']} missing success output")


def main():
    policy = load_json(POLICY_PATH)
    current_entries = require_entry_list(policy, "current")
    future_entries = require_entry_list(policy, "future")
    current_names = {entry["name"] for entry in current_entries}
    future_names = {entry["name"] for entry in future_entries}
    if current_names & future_names:
        raise FixtureError("current and future inventories overlap")

    cases = validate_fixtures(load_json(FIXTURES_PATH), future_names)
    validate_document(current_entries, future_entries)
    with tempfile.TemporaryDirectory(prefix="agl151-boundary-") as directory:
        temporary_dir = Path(directory)
        for case in cases:
            run_case(case, current_entries, temporary_dir)
    print(f"architecture-boundary-tests: ok cases={len(cases)}")


if __name__ == "__main__":
    try:
        main()
    except (FixtureError, OSError, subprocess.SubprocessError) as error:
        print(f"architecture-boundary-tests: error: {error}", file=sys.stderr)
        sys.exit(1)
