#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/extract-agl-terminal-history.sh DESTINATION [SOURCE_REF]

Creates a new local agl-terminal repository with filtered history for the
reviewed terminal package seam. SOURCE_REF defaults to HEAD. The source
repository is never rewritten. DESTINATION must not already exist.

The result has one branch named main, no remote, no imported tags, and no
refs/original namespace. Only the seven terminal package paths selected by
AGL-151 are retained.
EOF
}

fail() {
  printf 'extract-agl-terminal-history: %s\n' "$*" >&2
  exit 1
}

[[ $# -ge 1 && $# -le 2 ]] || {
  usage >&2
  exit 2
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source_root="$(git -C "$script_dir/.." rev-parse --show-toplevel)"
destination="$1"
source_ref="${2:-HEAD}"

[[ ! -e "$destination" ]] || fail "destination already exists: $destination"
[[ -z "$(git -C "$source_root" status --porcelain --untracked-files=no)" ]] ||
  fail "source tracked files must be clean"

source_commit="$(git -C "$source_root" rev-parse --verify "$source_ref^{commit}")" ||
  fail "source ref is not a commit: $source_ref"
source_branch="$(git -C "$source_root" branch --show-current)"
[[ -n "$source_branch" ]] || fail "source must be on a named branch"
[[ "$(git -C "$source_root" rev-parse --verify "$source_branch^{commit}")" == "$source_commit" ]] ||
  fail "SOURCE_REF must name the checked-out branch tip"

git clone \
  --no-hardlinks \
  --no-tags \
  --single-branch \
  --branch "$source_branch" \
  "$source_root" \
  "$destination"

filter_script="$source_root/scripts/ci/filter-agl-terminal-index.sh"
[[ -x "$filter_script" ]] || fail "filter helper is not executable: $filter_script"

FILTER_BRANCH_SQUELCH_WARNING=1 git -C "$destination" filter-branch \
  --force \
  --prune-empty \
  --index-filter "$filter_script" \
  -- "$source_branch"

while IFS= read -r original_ref; do
  git -C "$destination" update-ref -d "$original_ref"
done < <(git -C "$destination" for-each-ref --format='%(refname)' refs/original/)

git -C "$destination" remote remove origin
git -C "$destination" branch -m main
git -C "$destination" reflog expire --expire=now --all
git -C "$destination" gc --prune=now

filtered_commit="$(git -C "$destination" rev-parse --verify HEAD)"
path_count="$(git -C "$destination" ls-tree -r --name-only HEAD | wc -l)"

printf 'source_commit=%s\n' "$source_commit"
printf 'filtered_commit=%s\n' "$filtered_commit"
printf 'retained_paths=%s\n' "$path_count"
printf 'destination=%s\n' "$(cd -- "$destination" && pwd)"
