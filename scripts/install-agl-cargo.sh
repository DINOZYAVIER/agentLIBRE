#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/install-agl-cargo.sh [options]

Builds and installs `agl`, `agl-execd`, its private launcher, and the private
llama.cpp runtime directly under one prefix. Existing files are replaced only
with --force (the default).

Options:
  --root PATH          installation prefix (default: Cargo install root)
  --debug              use Cargo's debug install profile
  --no-force           refuse to replace an installed `agl`
  --no-locked          do not pass --locked to Cargo
  --skip-submodules    do not initialize vendor/llama.cpp
  --skip-llama-build   require an existing llama.cpp build
  --dry-run            print commands without executing them
  -h, --help           show this help
EOF
}

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  if [[ "$dry_run" -eq 0 ]]; then
    "$@"
  fi
}

root=""
debug=0
force=1
locked=1
skip_submodules=0
skip_llama_build=0
dry_run=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root) root="${2:?missing value for --root}"; shift 2 ;;
    --debug) debug=1; shift ;;
    --no-force) force=0; shift ;;
    --no-locked) locked=0; shift ;;
    --skip-submodules) skip_submodules=1; shift ;;
    --skip-llama-build) skip_llama_build=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$root" ]]; then
  root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-${HOME:?HOME is required}/.cargo}}"
fi
[[ "$root" == /* ]] || root="$PWD/$root"
root="$(realpath -m -s -- "$root")"
[[ "$(realpath -m -- "$root")" == "$root" ]] || {
  echo "installation prefix traverses a symlink: $root" >&2
  exit 1
}

engine_source="${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}/bin"
engine_target="$root/libexec/agentlibre"

if [[ "$dry_run" -eq 0 ]]; then
  command -v cargo >/dev/null || { echo "missing required tool: cargo" >&2; exit 1; }
  command -v git >/dev/null || { echo "missing required tool: git" >&2; exit 1; }
  if [[ "$skip_submodules" -eq 0 && ! -f "$repo_root/vendor/llama.cpp/CMakeLists.txt" ]]; then
    run git -C "$repo_root" submodule update --init --recursive --checkout vendor/llama.cpp
  fi
  if [[ ! -x "$engine_source/llama-server" ]]; then
    if [[ "$skip_llama_build" -eq 1 ]]; then
      echo "missing private llama-server: $engine_source/llama-server" >&2
      exit 1
    fi
    run "$repo_root/scripts/build-llama-cpp.sh"
  fi
fi

cargo_args=(install --path "$repo_root/products/agl-cli" --bin agl --root "$root")
[[ "$locked" -eq 1 ]] && cargo_args+=(--locked)
[[ "$debug" -eq 1 ]] && cargo_args+=(--debug)
[[ "$force" -eq 1 ]] && cargo_args+=(--force)

run cargo "${cargo_args[@]}"
execd_profile=release
[[ "$debug" -eq 1 ]] && execd_profile=debug
execd_args=(build --package agl-execd)
[[ "$locked" -eq 1 ]] && execd_args+=(--locked)
[[ "$debug" -eq 0 ]] && execd_args+=(--release)
run cargo "${execd_args[@]}"
run install -d -m 0755 "$root/bin" "$engine_target"
run install -m 0755 "$repo_root/target/$execd_profile/agl-execd" "$root/bin/agl-execd"
run install -m 0755 "$repo_root/target/$execd_profile/agl-execd-launcher" \
  "$engine_target/agl-execd-launcher"
run install -m 0755 "$engine_source/llama-server" "$engine_target/llama-server"

if [[ "$dry_run" -eq 1 ]]; then
  printf '+ install -m 0644 %q/lib\*.so\* %q/\n' "$engine_source" "$engine_target"
else
  shopt -s nullglob
  libraries=("$engine_source"/lib*.so*)
  shopt -u nullglob
  [[ ${#libraries[@]} -gt 0 ]] || {
    echo "private llama.cpp build has no shared libraries: $engine_source" >&2
    exit 1
  }
  run install -m 0644 "${libraries[@]}" "$engine_target/"
fi

echo "installed agl: $root/bin/agl"
echo "installed execd: $root/bin/agl-execd"
echo "installed private launcher: $engine_target/agl-execd-launcher"
echo "installed private engine: $engine_target/llama-server"
