#!/usr/bin/env python3
import json
import subprocess
import sys
from pathlib import Path

EXPECTED = {
    "agl-exec",
    "agl-process-launcher",
    "agl-pty",
    "agl-terminal",
    "agl-terminal-client",
    "agl-terminal-protocol",
    "agl-terminal-ui",
    "agl-terminald",
}

ALLOWED_INTERNAL = {
    "agl-exec",
    "agl-pty",
    "agl-terminal",
    "agl-terminal-client",
    "agl-terminal-protocol",
    "agl-terminald",
}

UI_AGENT_DEPENDENCIES = {
    "agl-client",
    "agl-content",
    "agl-ids",
    "agl-protocol",
}

def main() -> int:
    root = Path(__file__).resolve().parents[3]
    completed = subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )
    metadata = json.loads(completed.stdout)
    workspace_ids = set(metadata["workspace_members"])
    packages = {package["id"]: package for package in metadata["packages"]}
    members = {
        packages[package_id]["name"]: packages[package_id]
        for package_id in workspace_ids
        if packages[package_id]["name"] in EXPECTED
    }
    errors: list[str] = []

    if set(members) != EXPECTED:
        errors.append(
            "workspace package set differs: expected="
            f"{sorted(EXPECTED)!r} actual={sorted(members)!r}"
        )

    root_resolved = root.resolve()
    for name, package in sorted(members.items()):
        if package.get("source") is not None:
            errors.append(f"{name}: workspace member unexpectedly has an external source")
        manifest = Path(package["manifest_path"]).resolve()
        if root_resolved not in manifest.parents:
            errors.append(f"{name}: manifest escapes repository root: {manifest}")
        for dependency in package["dependencies"]:
            dependency_name = dependency["name"]
            dependency_path = dependency.get("path")
            source = dependency.get("source")
            is_ui_agent_dependency = (
                name == "agl-terminal-ui" and dependency_name in UI_AGENT_DEPENDENCIES
            )
            if (
                dependency_name.startswith("agl-")
                and dependency_name not in ALLOWED_INTERNAL
                and not is_ui_agent_dependency
            ):
                errors.append(f"{name}: forbidden agent package dependency {dependency_name}")
            if dependency_path is not None:
                resolved = Path(dependency_path).resolve()
                if root_resolved not in resolved.parents:
                    errors.append(f"{name}: dependency path escapes repository: {resolved}")
            if source is not None and str(source).startswith("git+"):
                errors.append(f"{name}: reintegrated package has Git dependency {source}")

    if errors:
        for error in sorted(errors):
            print(f"boundary: {error}", file=sys.stderr)
        return 1
    print(
        "terminal-boundary: ok "
        f"packages={len(members)} ui_agent_dependencies={len(UI_AGENT_DEPENDENCIES)} "
        "engine_service_agent_dependencies=0"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
