#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/terminal/install.sh [--prefix PATH] [--debug] [--dry-run]

Builds and installs one immutable agl-terminal generation. One atomic current
pointer selects the service, UI and private launcher as a unit.
EOF
}

fail() {
  printf 'agl-terminal-install: %s\n' "$*" >&2
  exit 1
}

fault_inject() {
  local point="$1"
  [[ "${AGL_TERMINAL_INSTALL_FAULT_AT:-}" != "$point" ]] ||
    fail "injected terminal installer fault at $point"
}

prefix=""
profile="release"
dry_run=0
while (($#)); do
  case "$1" in
    --prefix)
      shift
      (($#)) || fail "--prefix requires a path"
      prefix="$1"
      ;;
    --debug) profile="debug" ;;
    --dry-run) dry_run=1 ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) fail "unknown option: $1" ;;
  esac
  shift
done

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd)"
prefix="${prefix:-${HOME:?HOME is required}/.local}"
[[ "$prefix" == /* ]] || prefix="$PWD/$prefix"
prefix="$(realpath -m -s -- "$prefix")"
[[ "$(realpath -m -- "$prefix")" == "$prefix" ]] ||
  fail "prefix traverses a symlink: $prefix"

source_revision="$(git -C "$repo_root" rev-parse --verify HEAD)"
require_clean_source() {
  [[ "$(git -C "$repo_root" rev-parse --verify HEAD)" == "$source_revision" ]] ||
    fail "source revision changed during terminal installation"
  [[ -z "$(git -C "$repo_root" status --porcelain=v1 --untracked-files=normal --ignore-submodules=none)" ]] ||
    fail "terminal installation requires one clean root workspace"
}
build_args=(
  build --locked
  -p agl-process-launcher --bin agl-process-launcher
  -p agl-terminald --bin agl-terminald
  -p agl-terminal-ui --bin agl-terminal
)
[[ "$profile" == "debug" ]] || build_args+=(--release)
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
[[ "$target_dir" == /* ]] || target_dir="$repo_root/$target_dir"
binary_dir="$target_dir/$profile"

if ((dry_run)); then
  printf '+ cargo'
  printf ' %q' "${build_args[@]}"
  printf '\n'
  printf '+ install immutable generation below %q/libexec/agl-terminal/generations\n' "$prefix"
  printf '+ atomically select it through %q/libexec/agl-terminal/current\n' "$prefix"
  exit 0
fi

command -v cargo >/dev/null 2>&1 || fail "cargo is required"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required"
require_clean_source
cd "$repo_root"
cargo "${build_args[@]}"
require_clean_source

service="$binary_dir/agl-terminald"
launcher="$binary_dir/agl-process-launcher"
ui="$binary_dir/agl-terminal"
[[ -f "$service" && -x "$service" ]] || fail "missing built service: $service"
[[ -f "$launcher" && -x "$launcher" ]] || fail "missing built launcher: $launcher"
[[ -f "$ui" && -x "$ui" ]] || fail "missing built UI: $ui"
service_digest="sha256:$(sha256sum -- "$service" | awk '{print $1}')"
launcher_digest="sha256:$(sha256sum -- "$launcher" | awk '{print $1}')"
ui_digest="sha256:$(sha256sum -- "$ui" | awk '{print $1}')"
service_size="$(stat -c '%s' -- "$service")"
launcher_size="$(stat -c '%s' -- "$launcher")"
ui_size="$(stat -c '%s' -- "$ui")"
terminal_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/crates/agl-terminal-protocol/Cargo.toml" | head -n1)"
[[ -n "$terminal_version" ]] || fail "cannot resolve terminal product version"
product_root="$prefix/libexec/agl-terminal"
generation_root="$product_root/generations"
mkdir -p -- "$prefix/bin" "$generation_root"
for managed_directory in \
  "$prefix" \
  "$prefix/bin" \
  "$prefix/libexec" \
  "$product_root" \
  "$generation_root"
do
  [[ -d "$managed_directory" && ! -L "$managed_directory" &&
    "$(realpath -e -- "$managed_directory")" == "$managed_directory" &&
    "$(stat -c '%u' -- "$managed_directory")" == "$(id -u)" ]] ||
    fail "terminal managed directory is unsafe: $managed_directory"
  managed_mode="$(stat -c '%a' -- "$managed_directory")"
  (( (8#$managed_mode & 0022) == 0 )) ||
    fail "terminal managed directory is group/other writable: $managed_directory"
done
operation_lock="$product_root/.operation.lock"
[[ ! -L "$operation_lock" && ( ! -e "$operation_lock" || -f "$operation_lock" ) ]] ||
  fail "terminal operation lock is not a regular file: $operation_lock"
if [[ ! -e "$operation_lock" ]]; then
  (umask 077 && : >"$operation_lock")
fi
[[ "$(stat -c '%u:%a:%h' -- "$operation_lock")" == "$(id -u):600:1" ]] ||
  fail "terminal operation lock is not private and single-link: $operation_lock"
exec 9<>"$operation_lock"
flock -xn 9 || fail "another terminal install/uninstall operation holds $operation_lock"
stage="$(mktemp -d "$generation_root/.staging.XXXXXX")"
temporary_current=""

cleanup() {
  if [[ -d "$stage" ]]; then
    chmod u+w "$stage" 2>/dev/null || true
    rm -rf -- "$stage"
  fi
  if [[ -n "$temporary_current" ]]; then
    rm -f -- "$temporary_current"
  fi
}
trap cleanup EXIT

install -m 0555 -- "$service" "$stage/agl-terminald"
install -m 0555 -- "$launcher" "$stage/agl-process-launcher"
install -m 0555 -- "$ui" "$stage/agl-terminal"
printf '{\n  "schema": "agl-terminal.runtime-generation.v2",\n  "product_version": "%s",\n  "source_revision": "%s",\n  "protocol_version": 2,\n  "files": [\n    {\n      "role": "service",\n      "path": "agl-terminald",\n      "byte_size": %s,\n      "sha256": "%s"\n    },\n    {\n      "role": "launcher",\n      "path": "agl-process-launcher",\n      "byte_size": %s,\n      "sha256": "%s"\n    },\n    {\n      "role": "ui",\n      "path": "agl-terminal",\n      "byte_size": %s,\n      "sha256": "%s"\n    }\n  ]\n}\n' \
  "$terminal_version" "$source_revision" \
  "$service_size" "$service_digest" \
  "$launcher_size" "$launcher_digest" \
  "$ui_size" "$ui_digest" >"$stage/runtime-manifest.json"
chmod 0444 "$stage/runtime-manifest.json"
chmod 0555 "$stage"
manifest_digest="sha256:$(sha256sum -- "$stage/runtime-manifest.json" | awk '{print $1}')"
generation="generation-${manifest_digest#sha256:}"
destination="$generation_root/$generation"

if [[ -e "$destination" || -L "$destination" ]]; then
  [[ -d "$destination" && ! -L "$destination" ]] ||
    fail "existing generation is not a directory: $destination"
  for name in agl-terminald agl-process-launcher agl-terminal runtime-manifest.json; do
    cmp -- "$stage/$name" "$destination/$name" >/dev/null ||
      fail "existing generation content differs: $destination/$name"
  done
  [[ "$(find "$destination" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort)" == $'agl-process-launcher\nagl-terminal\nagl-terminald\nruntime-manifest.json' ]] ||
    fail "existing generation inventory is not canonical: $destination"
  chmod u+w "$stage"
  rm -rf -- "$stage"
  stage=""
else
  mv -- "$stage" "$destination"
  stage=""
fi
fault_inject after-generation-ready

for public_name in agl-terminal agl-terminald; do
  public_link="$prefix/bin/$public_name"
  if [[ -e "$public_link" && ! -L "$public_link" ]]; then
    fail "refusing to replace non-symlink public command: $public_link"
  fi
done
for public_name in agl-terminal agl-terminald; do
  public_link="$prefix/bin/$public_name"
  [[ -e "$public_link" || -L "$public_link" ]] ||
    ln -s -- "../libexec/agl-terminal/current/$public_name" "$public_link"
  [[ "$(readlink -- "$public_link")" == "../libexec/agl-terminal/current/$public_name" ]] ||
    fail "managed public command has an unexpected target: $public_link"
done
temporary_current="$product_root/.current.$$.tmp"
ln -s -- "generations/$generation" "$temporary_current"
fault_inject before-current-publish
mv -T -- "$temporary_current" "$product_root/current"
temporary_current=""

printf 'generation=%s\n' "$destination"
printf 'manifest_digest=%s\n' "$manifest_digest"
printf 'service_build_id=%s\n' "$service_digest"
printf 'launcher=%s\n' "$destination/agl-process-launcher"
printf 'ui=%s\n' "$prefix/bin/agl-terminal"
