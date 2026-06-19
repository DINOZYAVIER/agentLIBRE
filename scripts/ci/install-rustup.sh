#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

toolchain="${RUSTUP_TOOLCHAIN:-stable}"

if ! command -v rustup >/dev/null 2>&1; then
  ci_need_tool curl
  ci_section "Installing rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
  export PATH="$HOME/.cargo/bin:$PATH"
fi

ci_need_tool rustup

ci_section "Installing Rust toolchain"
ci_run rustup toolchain install "$toolchain" --profile minimal --component rustfmt --component clippy
ci_run rustup default "$toolchain"

if [[ -n "${GITHUB_PATH:-}" ]]; then
  printf '%s\n' "$HOME/.cargo/bin" >>"$GITHUB_PATH"
fi

ci_run rustc --version
ci_run cargo --version
