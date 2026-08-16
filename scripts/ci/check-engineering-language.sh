#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/../.." && pwd)"

english_term="con""tract"
russian_term="конт""ракт"
failed=0

path_is_first_party() {
  local path="$1"

  case "$path" in
    vendor/* | LICENSES/* | assets/core-skills | assets/core-skills/* | .agl/tasks | .agl/tasks/*)
      return 1
      ;;
  esac
}

scan_repository() {
  local repository="$1"
  local label="$2"
  local path
  local relative
  local -a files=()

  while IFS= read -r -d '' relative; do
    [[ -e "${repository}/${relative}" || -L "${repository}/${relative}" ]] || continue
    path_is_first_party "$relative" || continue

    if [[ "${relative,,}" == *"${english_term}"* || "$relative" == *"${russian_term}"* ]]; then
      printf '%s: disallowed path: %s\n' "$label" "$relative" >&2
      failed=1
    fi
    [[ -f "${repository}/${relative}" ]] && files+=("$relative")
  done < <(git -C "$repository" ls-files -co --exclude-standard -z)

  ((${#files[@]} == 0)) && return

  if git -C "$repository" grep --no-index -I -n -i \
    -e "$english_term" -e "$russian_term" -- "${files[@]}"; then
    printf '%s: disallowed engineering language found\n' "$label" >&2
    failed=1
  else
    local grep_status=$?
    if ((grep_status != 1)); then
      printf '%s: language scan failed with status %d\n' "$label" "$grep_status" >&2
      failed=1
    fi
  fi
}

scan_child_repository() {
  local relative="$1"
  local repository="${repo_root}/${relative}"
  local top_level

  [[ -d "$repository" ]] || return 0
  top_level="$(git -C "$repository" rev-parse --show-toplevel 2>/dev/null || true)"
  [[ "$top_level" == "$repository" ]] || return 0
  scan_repository "$repository" "$relative"
}

scan_repository "$repo_root" "workspace"

for child in \
  .agl/tasks \
  .agl/decision-docs \
  .agl/reviews \
  .agl/engineering-records; do
  scan_child_repository "$child"
done

if ((failed != 0)); then
  exit 1
fi

printf 'engineering language: ok\n'
