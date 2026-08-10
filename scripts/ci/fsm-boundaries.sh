#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

ci_need_tool git
ci_cd_repo

ci_section "FSM boundary guard"

failures=0

check_absent() {
  local description="$1"
  shift
  local output
  set +e
  output="$(git grep -n "$@" 2>/dev/null)"
  local status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    printf 'fsm boundary violation: %s\n%s\n' "$description" "$output" >&2
    failures=1
  elif [[ "$status" -gt 1 ]]; then
    printf 'fsm boundary check failed while scanning: %s\n' "$description" >&2
    failures=1
  fi
}

check_absent \
  "inference observation events must be emitted from transition records" \
  'InferenceObservationEvent::' -- \
  crates/agl-inference/src \
  ':(exclude)crates/agl-inference/src/evidence/event.rs' \
  ':(exclude)crates/agl-inference/src/evidence/tests.rs'

check_absent \
  "InferenceEventWriter must not expose a direct append(event) API" \
  -E 'pub fn append[[:space:]]*\(|\.append[[:space:]]*\(&InferenceObservationEvent' -- \
  crates/agl-inference/src

if [[ "$failures" -ne 0 ]]; then
  exit 1
fi

printf 'fsm boundary guard passed\n'
