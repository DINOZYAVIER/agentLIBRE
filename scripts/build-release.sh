#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
user_home="${HOME:?HOME is required}"
install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$user_home/.cargo}}"

usage() {
  cat <<'EOF'
Usage: scripts/build-release.sh

Builds the complete locked release, the private llama.cpp engine, and deploys
the binaries and engine libraries to the Cargo installation prefix used by the
user-systemd services.

The services are not restarted automatically. Run scripts/restart-services.sh
after a successful build.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi
if [[ $# -ne 0 ]]; then
  echo "unknown argument: $1" >&2
  usage >&2
  exit 2
fi

if [[ "${AGL_NIX_VULKAN_ACTIVE:-0}" != 1 ]]; then
  exec "$script_dir/agl-nix-vulkan.sh" -- "$script_dir/build-release.sh"
fi

command -v cargo >/dev/null 2>&1 || { echo "missing required tool: cargo" >&2; exit 1; }
command -v git >/dev/null 2>&1 || { echo "missing required tool: git" >&2; exit 1; }
command -v install >/dev/null 2>&1 || { echo "missing required tool: install" >&2; exit 1; }

if [[ ! -f "$repo_root/vendor/llama.cpp/CMakeLists.txt" ]]; then
  git -C "$repo_root" submodule update --init --recursive --checkout vendor/llama.cpp
fi

cd "$repo_root"
cargo build --locked --release --workspace
"$script_dir/build-llama-cpp.sh"

install_root="$(realpath -m -s -- "$install_root")"
[[ "$install_root" == /* ]] || {
  echo "installation prefix must be absolute: $install_root" >&2
  exit 2
}

engine_source="${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}/bin"
engine_target="$install_root/libexec/agentlibre"
install -d -m 0755 "$install_root/bin" "$engine_target"
install -m 0755 \
  target/release/agl \
  target/release/agl-execd \
  target/release/agl-execd-launcher \
  target/release/agl-matrix-bridge \
  "$install_root/bin/"
install -m 0755 "$engine_source/llama-server" "$engine_target/llama-server"

shopt -s nullglob
libraries=("$engine_source"/lib*.so*)
shopt -u nullglob
[[ ${#libraries[@]} -gt 0 ]] || {
  echo "private llama.cpp build has no shared libraries: $engine_source" >&2
  exit 1
}
install -m 0644 "${libraries[@]}" "$engine_target/"

echo "release deployed to $install_root"
echo "run: $script_dir/restart-services.sh"
