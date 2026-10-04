#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage: scripts/install-agl-cargo.sh [options]

Build and install agl, Matrix bridge and private
llama.cpp runtime under one prefix. Stage the complete runtime before replacing
installed files. With --services, also replace and restart user-systemd units.

Options:
  --root PATH          installation prefix (default: Cargo install root)
  --debug              build/install the Rust debug profile
  --no-force           refuse to replace installed runtime files
  --no-locked          do not pass --locked to Cargo
  --skip-submodules    do not initialize vendor/llama.cpp
  --skip-llama-build   require an existing llama.cpp build
  --services           replace user units and restart services after installation
  --no-services        install binaries only (default)
  --dry-run            print commands without executing them
  -h, --help           show this help
USAGE
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
profile=release
force=1
locked=1
skip_submodules=0
skip_llama_build=0
services=0
dry_run=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root) root="${2:?missing value for --root}"; shift 2 ;;
    --debug) profile=debug; shift ;;
    --no-force) force=0; shift ;;
    --no-locked) locked=0; shift ;;
    --skip-submodules) skip_submodules=1; shift ;;
    --skip-llama-build) skip_llama_build=1; shift ;;
    --services) services=1; shift ;;
    --no-services) services=0; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
root="${root:-${CARGO_INSTALL_ROOT:-${CARGO_HOME:-${HOME:?HOME is required}/.cargo}}}"
[[ "$root" == /* ]] || root="$PWD/$root"
root="$(realpath -m -s -- "$root")"
[[ "$(realpath -m -- "$root")" == "$root" ]] || {
  echo "installation prefix traverses a symlink: $root" >&2
  exit 1
}
engine_source="${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}/bin"
engine_target="$root/libexec/agentlibre"
rust_target="${CARGO_TARGET_DIR:-$repo_root/target}"
[[ "$rust_target" == /* ]] || rust_target="$repo_root/$rust_target"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
app_config_root="${AGL_HOME:+$AGL_HOME/config}"
app_config_root="${app_config_root:-$config_home/agentLIBRE}"
app_data_root="${AGL_HOME:+$AGL_HOME/data}"
app_data_root="${app_data_root:-${XDG_DATA_HOME:-$HOME/.local/share}/agentLIBRE}"
catalog="$app_data_root/source"
matrix_config="${AGL_MATRIX_BRIDGE_CONFIG:-$config_home/agentLIBRE/matrix-bridge/agl.toml}"

if [[ "$force" -eq 0 ]]; then
  for installed in "$root/bin/agl" "$root/bin/agl-matrix-bridge" "$engine_target"; do
    [[ ! -e "$installed" && ! -L "$installed" ]] || {
      echo "refusing to replace installed runtime: $installed" >&2
      exit 1
    }
  done
fi

if [[ "$dry_run" -eq 0 ]]; then
  for tool in cargo git install mv mktemp; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
  done
  if [[ "$services" -eq 1 ]]; then
    command -v systemctl >/dev/null || { echo "missing required tool: systemctl" >&2; exit 1; }
    systemctl --user show-environment >/dev/null
  fi
fi
if [[ "$skip_submodules" -eq 0 && ! -f "$repo_root/vendor/llama.cpp/CMakeLists.txt" ]]; then
  run git -C "$repo_root" submodule update --init --recursive --checkout vendor/llama.cpp
fi

# Resolve the workspace independently of the caller's directory.
cd "$repo_root"
cargo_args=(build --workspace)
[[ "$locked" -eq 1 ]] && cargo_args+=(--locked)
[[ "$profile" == release ]] && cargo_args+=(--release)
run cargo "${cargo_args[@]}"
if [[ "$skip_llama_build" -eq 0 ]]; then
  # An existing binary is not evidence that it matches the pinned source.
  run "$script_dir/build-llama-cpp.sh"
fi

shopt -s nullglob
libraries=("$engine_source"/lib*.so*)
shopt -u nullglob
if [[ "$dry_run" -eq 0 ]]; then
  for binary in agl agl-matrix-bridge; do
    [[ -x "$rust_target/$profile/$binary" ]] || { echo "missing built binary: $rust_target/$profile/$binary" >&2; exit 1; }
  done
  [[ -x "$engine_source/llama-server" ]] || { echo "missing private llama-server: $engine_source/llama-server" >&2; exit 1; }
  [[ ${#libraries[@]} -gt 0 ]] || { echo "private llama.cpp build has no shared libraries: $engine_source" >&2; exit 1; }
fi

run install -d -m 0755 "$root" "$root/bin" "$root/libexec"
stage="$root/.agentlibre-install.PREVIEW"
if [[ "$dry_run" -eq 0 ]]; then
  stage="$(mktemp -d "$root/.agentlibre-install.XXXXXX")"
  trap 'rm -rf -- "$stage"' EXIT
fi
run install -d -m 0755 "$stage/bin" "$stage/engine"
for binary in agl agl-matrix-bridge; do
  run install -m 0755 "$rust_target/$profile/$binary" "$stage/bin/$binary"
done
run install -m 0755 "$engine_source/llama-server" "$stage/engine/llama-server"
if [[ "$dry_run" -eq 1 ]]; then
  printf '+ install -m 0644 %q/lib\*.so\* %q/\n' "$engine_source" "$stage/engine"
else
  run install -m 0644 "${libraries[@]}" "$stage/engine/"
  "$stage/bin/agl" --version
  # Validate the relocated engine closure before interrupting active services.
  (exec 3>/dev/null; env -i AGL_LLAMA_SERVER_INVENTORY_FD=3 "$stage/engine/llama-server")
fi

# Seed only a new catalog. Updating binaries does not advance user pins.
manifest="$catalog/agentLIBRE.toml"
catalog_lock="$catalog/agentLIBRE.lock"
if [[ -e "$manifest" || -e "$catalog_lock" ]]; then
  [[ -f "$manifest" && -f "$catalog_lock" ]] || {
    echo "incomplete catalog: run agl forge sync from $catalog; existing files were preserved" >&2
    exit 1
  }
else
  run install -d -m 0755 "$catalog"
  run cp --no-clobber -- "$repo_root/assets/default-catalog.toml" "$manifest"
  if [[ "$dry_run" -eq 1 ]]; then
    printf '+ (cd %q && %q forge sync)\n' "$catalog" "$stage/bin/agl"
  else
    (cd "$catalog"; "$stage/bin/agl" forge sync)
    [[ -f "$catalog_lock" ]] || { echo "initial Forge sync did not publish $catalog_lock" >&2; exit 1; }
  fi
fi
settings="$app_config_root/agentLIBRE.toml"
if [[ ! -e "$settings" ]]; then
  run install -d -m 0700 "$app_config_root"
  if [[ "$dry_run" -eq 1 ]]; then
    printf '+ create CPU Small settings at %q\n' "$settings"
  else
    (umask 077; set -o noclobber; printf '[chat]\ndefault_function = "function:agentlibre.cpu-small@^1.0"\n' > "$settings")
  fi
fi

if [[ "$services" -eq 1 ]]; then
  run "$script_dir/restart-services.sh" --stop
fi
# Same-filesystem rename replaces running ELF files without truncating them.
for binary in agl agl-matrix-bridge; do
  run mv -fT -- "$stage/bin/$binary" "$root/bin/$binary"
done
run install -d -m 0755 "$engine_target"
# Replace managed flat files only. Runtime generations and their read-only
# dependency/GC-root directories are state, not stale engine libraries.
shopt -s nullglob
previous_libraries=("$engine_target"/lib*.so*)
shopt -u nullglob
for library in "${previous_libraries[@]}"; do
  if [[ ( -f "$library" || -L "$library" ) && ! -e "$stage/engine/${library##*/}" ]]; then
    run rm -- "$library"
  fi
done
if [[ "$dry_run" -eq 1 ]]; then
  printf '+ mv -f -- %q/* %q/\n' "$stage/engine" "$engine_target"
else
  for source in "$stage/engine/"*; do
    run mv -fT -- "$source" "$engine_target/${source##*/}"
  done
fi

if [[ "$services" -eq 1 ]]; then
  run "$script_dir/agentlibre-daemon-systemd-service.sh" --binary "$root/bin/agl" --enable
  if [[ -f "$matrix_config" ]]; then
    run "$script_dir/agentlibre-matrix-bridge-systemd-service.sh" --binary "$root/bin/agl-matrix-bridge" --config "$matrix_config" --enable
  fi
  run "$script_dir/restart-services.sh" --restart
fi
printf 'installed %s runtime: %s\n' "$profile" "$root"
