#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool git
ci_cd_repo

base_ref="${AGL_CI_DIFF_BASE:-}"
if [[ -z "$base_ref" && -n "${GITHUB_BASE_REF:-}" ]]; then
  base_ref="origin/$GITHUB_BASE_REF"
fi
if [[ -z "$base_ref" && -n "${GITEA_BASE_REF:-}" ]]; then
  base_ref="origin/$GITEA_BASE_REF"
fi
if [[ -z "$base_ref" && -n "${FORGEJO_BASE_REF:-}" ]]; then
  base_ref="origin/$FORGEJO_BASE_REF"
fi
if [[ -z "$base_ref" && -n "${CI_MERGE_REQUEST_TARGET_BRANCH_NAME:-}" ]]; then
  base_ref="origin/$CI_MERGE_REQUEST_TARGET_BRANCH_NAME"
fi
if [[ -z "$base_ref" && -n "${CI_COMMIT_TARGET_BRANCH:-}" ]]; then
  base_ref="origin/$CI_COMMIT_TARGET_BRANCH"
fi

if [[ -n "$base_ref" ]] && git rev-parse --verify --quiet "$base_ref^{commit}" >/dev/null; then
  ci_section "Checking whitespace in $base_ref...HEAD"
  ci_run git diff --check "$base_ref"...HEAD
elif git rev-parse --verify --quiet HEAD~1 >/dev/null; then
  ci_section "Checking whitespace in HEAD~1..HEAD"
  ci_run git diff --check HEAD~1..HEAD
else
  ci_section "Checking workspace whitespace"
  ci_run git diff --check
fi
