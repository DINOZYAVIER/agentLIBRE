#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage:
  scripts/install-agl-cargo.sh [options]

Installs the `agl` runtime bundle from this checkout with `cargo install`.

Options:
  --root PATH          Install under PATH instead of the resolved explicit root.
  --debug              Use Cargo's debug install profile.
  --no-force           Do not replace an existing installed `agl`.
  --no-locked          Do not pass --locked to cargo install.
  --skip-submodules    Do not initialize required git submodules.
  --skip-llama-build   Do not build llama.cpp before cargo install.
  --dry-run            Print the staged cargo install commands without running them.
  -h, --help           Show this help.

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
  if [[ "${dry_run:-0}" -eq 0 ]]; then
    "$@"
  fi
}

fail() {
  echo "$*" >&2
  exit 1
}

invocation_dir="$PWD"
cargo_root=""
debug=0
force=1
locked=1
skip_submodules=0
skip_llama_build=0
dry_run=0

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
    --dry-run)
      dry_run=1
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

if [[ -z "$cargo_root" ]]; then
  cargo_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-${HOME:?HOME is required}/.cargo}}"
fi
if [[ "$cargo_root" != /* ]]; then
  cargo_root="$invocation_dir/$cargo_root"
fi
cargo_root="$(realpath -m -s -- "$cargo_root")"
resolved_cargo_root="$(realpath -m -- "$cargo_root")"
[[ "$resolved_cargo_root" == "$cargo_root" ]] ||
  fail "refusing an install root whose path traverses a symlink: $cargo_root -> $resolved_cargo_root"

install_bin="$cargo_root/bin"
runtime_dir="$cargo_root/libexec/agentlibre"
generations_dir="$runtime_dir/generations"
current_link="$runtime_dir/current"
runtime_lock="$cargo_root/.agentlibre-runtime.lock"
installed_agl="$install_bin/agl"
installed_launcher="$install_bin/agl-process-launcher"
agl_link_target="../libexec/agentlibre/current/agl"
launcher_link_target="../libexec/agentlibre/current/agl-process-launcher"

cd "$repo_root"

if [[ "$dry_run" -eq 0 ]]; then
  need_tool cargo
  need_tool flock
  need_tool git
  need_tool sync
fi

if [[ "$dry_run" -eq 0 && "$skip_submodules" -eq 0 && -d "$repo_root/.git" ]]; then
  if [[ ! -f "$repo_root/assets/core-skills/agl/repo-status/SKILL.md" ]]; then
    run git submodule update --init assets/core-skills
  fi
  if [[ ! -f "$repo_root/vendor/llama.cpp/CMakeLists.txt" ]]; then
    run git submodule update --init --recursive vendor/llama.cpp
  fi
fi

if [[ "$dry_run" -eq 0 ]]; then
  missing_llama_lib=0
  llama_lib_dir="${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}/bin"
  for library in \
    libllama-common.so \
    libllama.so \
    libggml.so \
    libggml-base.so
  do
    if [[ ! -e "$llama_lib_dir/$library" ]]; then
      missing_llama_lib=1
      break
    fi
  done
  shopt -s nullglob
  cpu_backend_libraries=("$llama_lib_dir"/libggml-cpu-*.so)
  shopt -u nullglob
  if [[ ${#cpu_backend_libraries[@]} -eq 0 ]]; then
    missing_llama_lib=1
  fi

  if [[ "$missing_llama_lib" -eq 1 ]]; then
    if [[ "$skip_llama_build" -eq 1 ]]; then
      echo "missing llama.cpp libraries in $llama_lib_dir" >&2
      echo "run scripts/build-llama-cpp.sh or rerun without --skip-llama-build" >&2
      exit 1
    fi
    run "$repo_root/scripts/build-llama-cpp.sh"
  fi
fi

install_options=()
if [[ "$locked" -eq 1 ]]; then
  install_options+=(--locked)
fi
if [[ "$debug" -eq 1 ]]; then
  install_options+=(--debug)
fi

set_install_args() {
  local stage_root="$1"
  launcher_install_args=(
    install
    --path "$repo_root/crates/agl-process"
    --bin agl-process-launcher
    "${install_options[@]}"
    --root "$stage_root"
  )
  agl_install_args=(
    install
    --path "$repo_root/crates/agl-cli"
    --bin agl
    "${install_options[@]}"
    --root "$stage_root"
  )
}

if [[ "$dry_run" -eq 1 ]]; then
  dry_stage_root="$generations_dir/.staging.DRY-RUN/.cargo-root"
  set_install_args "$dry_stage_root"
  run cargo "${launcher_install_args[@]}"
  run cargo "${agl_install_args[@]}"
  printf '+ publish complete generation through %q\n' "$current_link"
  exit 0
fi

require_managed_directory() {
  local path="$1"
  local label="$2"
  local owner
  local mode
  if [[ -L "$path" ]]; then
    fail "refusing to use symlinked $label: $path"
  fi
  if [[ -e "$path" && ! -d "$path" ]]; then
    fail "refusing to use non-directory $label: $path"
  fi
  if [[ ! -d "$path" ]]; then
    (umask 022 && mkdir -p -- "$path")
    chmod 0755 -- "$path"
  fi
  [[ -d "$path" && ! -L "$path" ]] || fail "failed to create private $label: $path"
  owner="$(stat -c '%u' -- "$path")"
  [[ "$owner" == "$(id -u)" ]] ||
    fail "refusing $label not owned by the current UID: $path (owner $owner)"
  mode="$(stat -c '%a' -- "$path")"
  if (( (8#$mode & 0022) != 0 )); then
    fail "refusing group/other-writable $label: $path (mode $mode)"
  fi
}

require_managed_directory "$cargo_root" "install root"
require_managed_directory "$install_bin" "install bin directory"
require_managed_directory "$cargo_root/libexec" "libexec directory"
require_managed_directory "$runtime_dir" "runtime directory"
require_managed_directory "$generations_dir" "runtime generations directory"

if [[ -L "$runtime_lock" || ( -e "$runtime_lock" && ! -f "$runtime_lock" ) ]]; then
  fail "refusing to use non-regular runtime lock: $runtime_lock"
fi
if [[ ! -e "$runtime_lock" ]]; then
  (umask 077 && : >>"$runtime_lock")
fi
chmod 0600 "$runtime_lock"
exec {runtime_lock_fd}<>"$runtime_lock"
if ! flock --exclusive --nonblock "$runtime_lock_fd"; then
  fail "refusing to install while another runtime bundle operation holds: $runtime_lock"
fi

if [[ "$force" -eq 0 && ( -e "$installed_agl" || -L "$installed_agl" || -e "$installed_launcher" || -L "$installed_launcher" ) ]]; then
  fail "refusing to replace the existing runtime bundle because --no-force was requested"
fi

for install_artifact in "$installed_agl" "$installed_launcher" "$current_link"; do
  if [[ -e "$install_artifact" || -L "$install_artifact" ]]; then
    if [[ "$install_artifact" == "$current_link" ]]; then
      [[ -L "$install_artifact" ]] || fail "refusing to replace non-symlink current pointer: $install_artifact"
    elif [[ ! -f "$install_artifact" && ! -L "$install_artifact" ]]; then
      fail "refusing to replace non-file install artifact: $install_artifact"
    fi
  fi
done

generation_staging=""
pending_symlink=""

cleanup_transaction() {
  local status=$?
  trap '' HUP INT TERM
  trap - EXIT
  if [[ -n "$pending_symlink" ]]; then
    rm -f -- "$pending_symlink"
  fi
  if [[ -n "$generation_staging" ]]; then
    case "$generation_staging" in
      "$generations_dir"/.staging.*)
        chmod -R u+w -- "$generation_staging" 2>/dev/null || true
        rm -rf -- "$generation_staging"
        ;;
    esac
  fi
  exit "$status"
}

trap cleanup_transaction EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

fault_inject() {
  local point="$1"
  if [[ "${AGL_INSTALL_FAULT_AT:-}" == "$point" ]]; then
    echo "injecting installer fault at $point" >&2
    kill -KILL "$BASHPID"
  fi
}

sync_path() {
  sync -f "$1"
}

validate_generation() {
  local generation="$1"
  [[ -d "$generation" ]] || fail "runtime generation is not a directory: $generation"
  [[ -f "$generation/agl" && -x "$generation/agl" && ! -L "$generation/agl" ]] ||
    fail "runtime generation has no regular executable agl: $generation"
  [[ -f "$generation/agl-process-launcher" && -x "$generation/agl-process-launcher" && ! -L "$generation/agl-process-launcher" ]] ||
    fail "runtime generation has no regular executable process launcher: $generation"
}

resolve_current_generation() {
  local resolved
  [[ -L "$current_link" ]] || fail "runtime current pointer is missing: $current_link"
  resolved="$(readlink -f -- "$current_link")" || fail "runtime current pointer is broken: $current_link"
  [[ "$(dirname -- "$resolved")" == "$generations_dir" ]] ||
    fail "runtime current pointer escapes the generations directory: $current_link"
  validate_generation "$resolved"
  printf '%s\n' "$resolved"
}

atomic_symlink() {
  local target="$1"
  local destination="$2"
  pending_symlink="${destination}.agentlibre-new.$BASHPID.$RANDOM"
  [[ ! -e "$pending_symlink" && ! -L "$pending_symlink" ]] ||
    fail "temporary publication path already exists: $pending_symlink"
  ln -s -- "$target" "$pending_symlink"
  mv -fT -- "$pending_symlink" "$destination"
  pending_symlink=""
  sync_path "$(dirname -- "$destination")"
}

publish_current() {
  local generation="$1"
  validate_generation "$generation"
  atomic_symlink "generations/$(basename -- "$generation")" "$current_link"
}

managed_link_kind() {
  local path="$1"
  local target="$2"
  if [[ ! -e "$path" && ! -L "$path" ]]; then
    printf 'absent\n'
  elif [[ -L "$path" && "$(readlink -- "$path")" == "$target" ]]; then
    printf 'managed\n'
  elif [[ -f "$path" && ! -L "$path" ]]; then
    printf 'regular\n'
  else
    printf 'invalid\n'
  fi
}

install_managed_link() {
  local destination="$1"
  local target="$2"
  atomic_symlink "$target" "$destination"
}

classify_managed_surface() {
  local agl_kind
  local launcher_kind

  agl_kind="$(managed_link_kind "$installed_agl" "$agl_link_target")"
  launcher_kind="$(managed_link_kind "$installed_launcher" "$launcher_link_target")"
  if [[ "$agl_kind" == regular || "$launcher_kind" == regular ]]; then
    echo "refusing to replace an unmanaged regular runtime command under $install_bin" >&2
    echo "agentLIBRE alpha installs do not migrate flat binaries; move or remove both" >&2
    echo "  $installed_agl" >&2
    echo "  $installed_launcher" >&2
    fail "then rerun the installer"
  fi
  [[ "$agl_kind" != invalid ]] || fail "refusing to replace unmanaged agl artifact: $installed_agl"
  [[ "$launcher_kind" != invalid ]] || fail "refusing to replace unmanaged launcher artifact: $installed_launcher"

  if [[ ! -e "$current_link" && ! -L "$current_link" ]]; then
    if [[ "$agl_kind" == absent || "$agl_kind" == managed ]] &&
      [[ "$launcher_kind" == absent || "$launcher_kind" == managed ]]; then
      surface_mode="fresh"
      return 0
    fi
    fail "refusing an unsupported fresh-install surface under $install_bin"
  fi

  [[ -L "$current_link" ]] || fail "refusing to replace non-symlink current pointer: $current_link"
  resolve_current_generation >/dev/null
  if [[ "$agl_kind" == managed && "$launcher_kind" == managed ]]; then
    surface_mode="update"
    return 0
  fi

  fail "refusing an incomplete managed runtime surface under $install_bin; restore both managed links before retrying"
}

surface_mode=""
classify_managed_surface

generation_staging="$(mktemp -d "$generations_dir/.staging.XXXXXXXX")"
generation_token="${generation_staging##*.staging.}"
new_generation="$generations_dir/generation-$generation_token"
stage_root="$generation_staging/.cargo-root"
mkdir -p "$stage_root"
set_install_args "$stage_root"

run cargo "${launcher_install_args[@]}"
run cargo "${agl_install_args[@]}"

[[ -x "$stage_root/bin/agl" && ! -L "$stage_root/bin/agl" ]] ||
  fail "staged cargo install did not produce a regular executable agl"
[[ -x "$stage_root/bin/agl-process-launcher" && ! -L "$stage_root/bin/agl-process-launcher" ]] ||
  fail "staged cargo install did not produce a regular executable process launcher"
run "$stage_root/bin/agl" --version
run env AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE=1 "$stage_root/bin/agl"

mv -- "$stage_root/bin/agl" "$generation_staging/agl"
mv -- "$stage_root/bin/agl-process-launcher" "$generation_staging/agl-process-launcher"
rm -rf -- "$stage_root"
chmod 0555 "$generation_staging/agl" "$generation_staging/agl-process-launcher"
validate_generation "$generation_staging"
sync_path "$generation_staging"
chmod 0555 "$generation_staging"
mv -- "$generation_staging" "$new_generation"
generation_staging=""
sync_path "$generations_dir"
fault_inject after-generation-ready

if [[ "$surface_mode" == fresh ]]; then
  fault_inject before-initial-links
  if [[ ! -L "$installed_launcher" ]]; then
    install_managed_link "$installed_launcher" "$launcher_link_target"
  fi
  fault_inject after-initial-launcher-link
  if [[ ! -L "$installed_agl" ]]; then
    install_managed_link "$installed_agl" "$agl_link_target"
  fi
  fault_inject after-initial-agl-link
  fault_inject before-initial-current-publish
  publish_current "$new_generation"
  fault_inject after-initial-current-publish
else
  fault_inject before-new-current-publish
  publish_current "$new_generation"
  fault_inject after-new-current-publish
fi

resolved_agl="$(readlink -f -- "$installed_agl")"
resolved_launcher="$(readlink -f -- "$installed_launcher")"
[[ "$resolved_agl" == "$new_generation/agl" ]] ||
  fail "installed agl did not resolve through the published generation"
[[ "$resolved_launcher" == "$new_generation/agl-process-launcher" ]] ||
  fail "installed launcher did not resolve through the published generation"

echo "installed agl: $installed_agl -> $resolved_agl"
echo "installed process launcher: $installed_launcher -> $resolved_launcher"
run "$installed_agl" --version

trap - EXIT HUP INT TERM
if [[ ":$PATH:" != *":$install_bin:"* ]]; then
  echo "The install directory is not on PATH." >&2
  echo "Add this to your shell profile:" >&2
  echo "  export PATH=\"$install_bin:\$PATH\"" >&2
fi
