#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage:
  scripts/install-agl-cargo.sh [options]

Installs the `agl` CLI from this checkout with `cargo install`.

Options:
  --root PATH          Install under PATH instead of Cargo's default root.
  --debug             Use Cargo's debug install profile.
  --no-force          Do not replace an existing installed `agl`.
  --no-locked         Do not pass --locked to cargo install.
  --skip-submodules   Do not initialize required git submodules.
  --skip-llama-build  Do not build llama.cpp before cargo install.
  -h, --help          Show this help.

Examples:
  scripts/install-agl-cargo.sh
  scripts/install-agl-cargo.sh --root "$HOME/.cargo"
USAGE
}

need_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required tool: $1" >&2
    exit 1
  }
}

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

cargo_root=""
debug=0
force=1
locked=1
skip_submodules=0
skip_llama_build=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      [[ $# -ge 2 ]] || {
        echo "--root requires a path" >&2
        exit 2
      }
      cargo_root="$2"
      shift 2
      ;;
    --debug)
      debug=1
      shift
      ;;
    --no-force)
      force=0
      shift
      ;;
    --no-locked)
      locked=0
      shift
      ;;
    --skip-submodules)
      skip_submodules=1
      shift
      ;;
    --skip-llama-build)
      skip_llama_build=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

need_tool cargo
need_tool git

cd "$repo_root"

if [[ "$skip_submodules" -eq 0 && -d "$repo_root/.git" ]]; then
  if [[ ! -f "$repo_root/assets/core-skills/agl/repo-status/SKILL.md" ]]; then
    run git submodule update --init assets/core-skills
  fi
  if [[ ! -f "$repo_root/vendor/llama.cpp/CMakeLists.txt" ]]; then
    run git submodule update --init --recursive vendor/llama.cpp
  fi
fi

missing_llama_lib=0
llama_lib_dir="${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}/bin"
for library in \
  libllama-common.so \
  libllama.so \
  libggml.so \
  libggml-base.so \
  libggml-cpu.so \
  libggml-vulkan.so
do
  if [[ ! -e "$llama_lib_dir/$library" ]]; then
    missing_llama_lib=1
    break
  fi
done

if [[ "$missing_llama_lib" -eq 1 ]]; then
  if [[ "$skip_llama_build" -eq 1 ]]; then
    echo "missing llama.cpp libraries in $llama_lib_dir" >&2
    echo "run scripts/build-llama-cpp.sh or rerun without --skip-llama-build" >&2
    exit 1
  fi
  run "$repo_root/scripts/build-llama-cpp.sh"
fi

install_args=(
  install
  --path "$repo_root/crates/agl-cli"
  --bin agl
)

if [[ "$force" -eq 1 ]]; then
  install_args+=(--force)
fi
if [[ "$locked" -eq 1 ]]; then
  install_args+=(--locked)
fi
if [[ "$debug" -eq 1 ]]; then
  install_args+=(--debug)
fi
if [[ -n "$cargo_root" ]]; then
  install_args+=(--root "$cargo_root")
fi

run cargo "${install_args[@]}"

installed_agl="$(command -v agl || true)"
if [[ -z "$installed_agl" ]]; then
  default_bin="${cargo_root:-${CARGO_INSTALL_ROOT:-$HOME/.cargo}}/bin"
  echo "agl installed, but it is not on PATH." >&2
  echo "Add this to your shell profile:" >&2
  echo "  export PATH=\"$default_bin:\$PATH\"" >&2
  exit 0
fi

echo "installed agl: $installed_agl"
run agl --version
