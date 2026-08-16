#!/usr/bin/env python3
"""Reject tests that use implementation text as runtime evidence."""

from __future__ import annotations

import re
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
TEST_HELPER_NAMES = (
    "adapter_source",
    "production_rs_files",
    "production_sources",
    "rust_sources",
    "source_matches",
)
IMPLEMENTATION_READ = re.compile(
    r"(?:read_to_string|include_str!)\s*\([^;]{0,500}"
    r"(?:Cargo\.toml|crates[/\\][^\"']+[/\\]src|scripts[/\\][^\"']+\.sh|"
    r"vendor[/\\][^\"']+\.(?:c|cc|cpp|h|hpp)|[^\"']+\.rs)",
    re.DOTALL,
)


def test_sources() -> list[Path]:
    sources: list[Path] = []
    for path in (REPO_ROOT / "crates").rglob("*.rs"):
        relative = path.relative_to(REPO_ROOT)
        parts = relative.parts
        text = path.read_text(encoding="utf-8")
        if "tests" in parts or path.name == "tests.rs" or "#[cfg(test)]" in text:
            sources.append(path)
    return sorted(sources)


def main() -> int:
    failures: list[str] = []
    for path in test_sources():
        text = path.read_text(encoding="utf-8")
        if "#[cfg(test)]" in text and "tests" not in path.parts and path.name != "tests.rs":
            text = text[text.index("#[cfg(test)]") :]
        for helper in TEST_HELPER_NAMES:
            if re.search(rf"\b{re.escape(helper)}\b", text):
                failures.append(f"{path.relative_to(REPO_ROOT)}: implementation reader `{helper}`")
        for match in IMPLEMENTATION_READ.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            failures.append(
                f"{path.relative_to(REPO_ROOT)}:{line}: reads implementation text"
            )

    if failures:
        for failure in failures:
            print(f"behavior-test check: {failure}", file=sys.stderr)
        return 1

    print(f"behavior-test check: ok files={len(test_sources())}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
