#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../.." && pwd)"
revision="$(git -C "$repo_root" rev-parse --verify HEAD)"
temporary_root="$(mktemp -d)"
checkout="$temporary_root/agentLIBRE"

cleanup() {
  rm -rf -- "$temporary_root"
}
trap cleanup EXIT

git clone --no-local --no-tags "$repo_root" "$checkout" >/dev/null
git -C "$checkout" checkout --detach "$revision" >/dev/null

[[ ! -e "$checkout/../agl-terminal" ]] || {
  printf 'clean-checkout: unexpected sibling agl-terminal checkout\n' >&2
  exit 1
}

cargo test --locked --manifest-path "$checkout/Cargo.toml" \
  -p agl-exec -p agl-process-launcher -p agl-pty -p agl-terminal \
  -p agl-terminal-client -p agl-terminal-protocol -p agl-terminal-ui \
  -p agl-terminald
python3 "$checkout/scripts/terminal/ci/check-boundaries.py"

printf 'clean-checkout: passed revision=%s\n' "$revision"
