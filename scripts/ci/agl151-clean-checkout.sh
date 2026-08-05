#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
agent_root="$(cd -- "$script_dir/../.." && pwd)"
terminal_root="${AGL_TERMINAL_REPO:-/home/dinozyavier/repos/agl-terminal}"
temporary_root="$(mktemp -d)"
cleanup() {
  chmod -R u+w "$temporary_root" 2>/dev/null || true
  rm -rf -- "$temporary_root"
}
trap cleanup EXIT

agent_revision="$(git -C "$agent_root" rev-parse --verify HEAD)"
terminal_revision="$(git -C "$terminal_root" rev-parse --verify HEAD)"
declared_terminal_revision="$(sed -n 's/^agl-terminal = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' "$agent_root/Cargo.toml")"
[[ "$declared_terminal_revision" == "$terminal_revision" ]] || {
  printf 'agl151-clean-checkout: agent pin %s does not select terminal HEAD %s\n' \
    "$declared_terminal_revision" "$terminal_revision" >&2
  exit 1
}

agent_checkout="$temporary_root/agentLIBRE"
terminal_checkout="$temporary_root/agl-terminal"
git clone --no-local --no-tags "$agent_root" "$agent_checkout" >/dev/null
git clone --no-local --no-tags "$terminal_root" "$terminal_checkout" >/dev/null
git -C "$agent_checkout" checkout --detach "$agent_revision" >/dev/null
git -C "$terminal_checkout" checkout --detach "$terminal_revision" >/dev/null

export CARGO_HOME="$temporary_root/cargo-home"
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export GIT_CONFIG_COUNT=4
export GIT_CONFIG_KEY_0="url.file://$terminal_checkout.insteadOf"
export GIT_CONFIG_VALUE_0="file://$terminal_root"
export GIT_CONFIG_KEY_1="url.file://$agent_checkout.insteadOf"
export GIT_CONFIG_VALUE_1="file://$agent_root"
export GIT_CONFIG_KEY_2="url.file://$agent_root/assets/core-skills.insteadOf"
export GIT_CONFIG_VALUE_2="https://github.com/DINOZYAVIER/agl-core-skills.git"
export GIT_CONFIG_KEY_3="url.file://$agent_root/vendor/llama.cpp.insteadOf"
export GIT_CONFIG_VALUE_3="https://github.com/ggml-org/llama.cpp"

git -c protocol.file.allow=always -C "$agent_checkout" \
  submodule update --init --recursive >/dev/null

cargo test --locked --manifest-path "$agent_checkout/Cargo.toml" \
  -p agl-process -p agl-runtime -p agl-cli --lib
python3 "$agent_checkout/scripts/ci/check-architecture-boundaries.py"

cargo test --locked --manifest-path "$terminal_checkout/Cargo.toml" --workspace
python3 "$terminal_checkout/scripts/ci/check-boundaries.py"
python3 "$terminal_checkout/scripts/verify-provenance.py"
cargo build --locked --release --manifest-path "$terminal_checkout/Cargo.toml" \
  -p agl-terminald --bin agl-terminald
actual_build_id="sha256:$(sha256sum "$terminal_checkout/target/release/agl-terminald" | awk '{print $1}')"
# The agent pins the published release artifact, while this command proves a
# fresh source build. Rust binaries built under another absolute checkout root
# are not assumed to be byte-for-byte reproducible.
declared_build_id="$(sed -n 's/^[[:space:]]*"\(sha256:[0-9a-f]\{64\}\)";$/\1/p' \
  "$agent_checkout/crates/agl-process/src/lib.rs")"
[[ "$declared_build_id" == sha256:* ]] || {
  printf 'agl151-clean-checkout: agent has no exact terminal release build identity\n' >&2
  exit 1
}

[[ -z "$(git -C "$agent_checkout" status --porcelain)" &&
  -z "$(git -C "$terminal_checkout" status --porcelain)" ]] || {
  printf 'agl151-clean-checkout: verification mutated a source checkout\n' >&2
  exit 1
}
printf 'agl151-clean-checkout: passed agent=%s terminal=%s declared_build=%s clean_build=%s\n' \
  "$agent_revision" "$terminal_revision" "$declared_build_id" "$actual_build_id"
