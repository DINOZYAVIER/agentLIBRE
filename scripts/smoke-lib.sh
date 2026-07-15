#!/usr/bin/env bash

fail() {
  echo "smoke failed: $*" >&2
  exit 1
}

need_tool() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"
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

json_content() {
  python3 - "$1" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    print(json.load(handle)["content"], end="")
PY
}
