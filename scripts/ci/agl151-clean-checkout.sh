#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
agent_root="$(cd -- "$script_dir/../.." && pwd)"
temporary_root="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$temporary_root" 2>/dev/null || true
  rm -rf -- "$temporary_root"
}
trap cleanup EXIT

agent_revision="$(git -C "$agent_root" rev-parse --verify HEAD)"

agent_checkout="$temporary_root/agentLIBRE"
git clone --no-local --no-tags "$agent_root" "$agent_checkout" >/dev/null
git -C "$agent_checkout" checkout --detach "$agent_revision" >/dev/null

[[ ! -e "$temporary_root/agl-terminal" ]] || {
  printf 'agl151-clean-checkout: unexpected sibling agl-terminal checkout\n' >&2
  exit 1
}

export CARGO_HOME="$temporary_root/cargo-home"
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export GIT_CONFIG_COUNT=2
export GIT_CONFIG_KEY_0="url.file://$agent_root/assets/core-skills.insteadOf"
export GIT_CONFIG_VALUE_0="https://github.com/DINOZYAVIER/agl-core-skills.git"
export GIT_CONFIG_KEY_1="url.file://$agent_root/vendor/llama.cpp.insteadOf"
export GIT_CONFIG_VALUE_1="https://github.com/ggml-org/llama.cpp"

git -c protocol.file.allow=always -C "$agent_checkout" \
  submodule update --init --recursive >/dev/null

cargo test --locked --manifest-path "$agent_checkout/Cargo.toml" \
  -p agl-process -p agl-runtime -p agl-cli -p agl-exec \
  -p agl-process-launcher -p agl-pty -p agl-terminal \
  -p agl-terminal-client -p agl-terminal-protocol -p agl-terminal-ui \
  -p agl-terminald --lib
python3 "$agent_checkout/scripts/ci/check-architecture-boundaries.py"
python3 "$agent_checkout/scripts/terminal/ci/check-boundaries.py"

[[ -z "$(git -C "$agent_checkout" status --porcelain)" ]] || {
  printf 'agl151-clean-checkout: verification mutated a source checkout\n' >&2
  exit 1
}
printf 'agl151-clean-checkout: passed revision=%s terminal_source=in-tree\n' \
  "$agent_revision"
