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
forbidden_public_worker="$install_bin/agl-inference-worker"
agl_link_target="../libexec/agentlibre/current/agl"
launcher_link_target="../libexec/agentlibre/current/agl-process-launcher"

cd "$repo_root"

if [[ "$dry_run" -eq 0 ]]; then
  need_tool cargo
  need_tool flock
  need_tool git
  need_tool readelf
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
  cargo_profile=debug
else
  cargo_profile=release
fi
cargo_target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
if [[ "$cargo_target_dir" != /* ]]; then
  cargo_target_dir="$repo_root/$cargo_target_dir"
fi
native_bundle_build_base="$cargo_target_dir/$cargo_profile/agl-inference-native"

resolve_native_bundle_relative() {
  local worker="$1"
  local dynamic
  local runpath
  local component
  local relative
  local -a matches=()

  [[ -f "$worker" && ! -L "$worker" ]] ||
    fail "cannot resolve native bundle from a non-regular inference worker: $worker"
  dynamic="$(LC_ALL=C readelf -d -- "$worker")" ||
    fail "failed to inspect inference worker RUNPATH: $worker"
  while IFS= read -r runpath; do
    IFS=: read -r -a components <<<"$runpath"
    for component in "${components[@]}"; do
      if [[ "$component" == '$ORIGIN/'* ]]; then
        relative="${component#\$ORIGIN/}"
        if [[ "$relative" == agl-inference-native/* ]]; then
          [[ "$relative" =~ ^agl-inference-native/sha256-[0-9a-f]{64}$ ]] ||
            fail "inference worker names an invalid content-addressed native bundle: $component"
          matches+=("$relative")
        fi
      fi
    done
  done < <(sed -n -E 's/.*\((RPATH|RUNPATH)\).*\[(.*)\]/\2/p' <<<"$dynamic")

  if (( ${#matches[@]} != 1 )); then
    fail "inference worker must select exactly one content-addressed native bundle: $worker"
  fi
  printf '%s\n' "${matches[0]}"
}

set_install_args() {
  local stage_root="$1"
  launcher_install_args=(
    install
    --path "$repo_root/crates/agl-process"
    --bin agl-process-launcher
    "${install_options[@]}"
    --root "$stage_root"
  )
  worker_install_args=(
    install
    --path "$repo_root/crates/agl-inference-worker"
    --bin agl-inference-worker
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
  run cargo "${worker_install_args[@]}"
  run cargo "${agl_install_args[@]}"
  printf '+ resolve exact content-addressed native bundle from %q\n' \
    "$dry_stage_root/bin/agl-inference-worker"
  printf '+ copy selected %q/sha256-DIGEST to %q\n' \
    "$native_bundle_build_base" "$dry_stage_root/bin/agl-inference-native"
  printf '+ pin exact Nix runtime references below final generation .nix-gc-roots\n'
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

if [[ -e "$forbidden_public_worker" || -L "$forbidden_public_worker" ]]; then
  fail "refusing a public inference worker command; remove it before installing the private runtime bundle: $forbidden_public_worker"
fi

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
unpublished_generation=""
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
  if [[ -n "$unpublished_generation" ]]; then
    case "$unpublished_generation" in
      "$generations_dir"/generation-*)
        current_generation="$(readlink -f -- "$current_link" 2>/dev/null || true)"
        if [[ "$current_generation" != "$unpublished_generation" ]]; then
          chmod u+rwx -- "$unpublished_generation" 2>/dev/null || true
          chmod -R u+rwX -- "$unpublished_generation/agl-inference-native" 2>/dev/null || true
          chmod u+rwx -- "$unpublished_generation/.nix-gc-roots" 2>/dev/null || true
          rm -rf -- "$unpublished_generation"
        fi
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
  local executable
  [[ -d "$generation" && ! -L "$generation" &&
    "$(stat -c '%a' -- "$generation")" == 555 &&
    "$(stat -c '%u' -- "$generation")" == "$(id -u)" ]] ||
    fail "runtime generation is not an exact immutable directory: $generation"
  [[ -f "$generation/agl" && -x "$generation/agl" && ! -L "$generation/agl" ]] ||
    fail "runtime generation has no regular executable agl: $generation"
  [[ -f "$generation/agl-process-launcher" && -x "$generation/agl-process-launcher" && ! -L "$generation/agl-process-launcher" ]] ||
    fail "runtime generation has no regular executable process launcher: $generation"
  [[ -f "$generation/agl-inference-worker" && -x "$generation/agl-inference-worker" && ! -L "$generation/agl-inference-worker" ]] ||
    fail "runtime generation has no regular executable inference worker: $generation"
  for executable in \
    "$generation/agl" \
    "$generation/agl-process-launcher" \
    "$generation/agl-inference-worker"
  do
    [[ "$(stat -c '%a' -- "$executable")" == 555 &&
      "$(stat -c '%u' -- "$executable")" == "$(id -u)" &&
      "$(stat -c '%h' -- "$executable")" == 1 ]] ||
      fail "runtime generation executable is not an exact immutable single-link file: $executable"
  done
  validate_native_bundle \
    "$generation/agl-inference-native" \
    "$generation/agl-inference-worker"
}

is_obsolete_two_binary_generation() {
  local generation="$1"
  local entry
  local name
  local entry_count=0
  [[ -d "$generation" && ! -L "$generation" &&
    "$(stat -c '%a' -- "$generation")" == 555 &&
    "$(stat -c '%u' -- "$generation")" == "$(id -u)" ]] ||
    return 1
  for name in agl agl-process-launcher; do
    entry="$generation/$name"
    [[ -f "$entry" && -x "$entry" && ! -L "$entry" &&
      "$(stat -c '%a' -- "$entry")" == 555 &&
      "$(stat -c '%u' -- "$entry")" == "$(id -u)" &&
      "$(stat -c '%h' -- "$entry")" == 1 ]] ||
      return 1
  done
  shopt -s nullglob
  for entry in "$generation"/* "$generation"/.[!.]* "$generation"/..?*; do
    case "${entry##*/}" in
      agl | agl-process-launcher) ;;
      *)
        shopt -u nullglob
        return 1
        ;;
    esac
    entry_count=$((entry_count + 1))
  done
  shopt -u nullglob
  (( entry_count == 2 ))
}

reject_obsolete_two_binary_generation() {
  local generation="$1"
  is_obsolete_two_binary_generation "$generation" || return 0
  echo "existing managed runtime uses an obsolete two-binary alpha layout" >&2
  echo "agentLIBRE alpha installers do not migrate obsolete runtime generations" >&2
  echo "move or remove all of these managed artifacts before a clean install:" >&2
  echo "  $installed_agl" >&2
  echo "  $installed_launcher" >&2
  echo "  $current_link" >&2
  echo "  $generation" >&2
  echo "preview and remove the complete managed surface with:" >&2
  printf '  %q --root %q\n' "$script_dir/uninstall-agl-cargo.sh" "$cargo_root" >&2
  printf '  %q --root %q --apply\n' "$script_dir/uninstall-agl-cargo.sh" "$cargo_root" >&2
  fail "then rerun the installer"
}

validate_native_bundle() {
  local base="$1"
  local worker="$2"
  local relative
  local leaf_name
  local directory
  local base_entry
  local base_count=0
  [[ -d "$base" && ! -L "$base" ]] ||
    fail "runtime generation has no exact native inference bundle base: $base"
  [[ "$(stat -c '%a' -- "$base")" == 555 ]] ||
    fail "native inference bundle base is not immutable: $base"
  [[ "$(stat -c '%u' -- "$base")" == "$(id -u)" ]] ||
    fail "native inference bundle base has the wrong owner: $base"
  relative="$(resolve_native_bundle_relative "$worker")"
  leaf_name="${relative#agl-inference-native/}"
  shopt -s nullglob
  for base_entry in "$base"/* "$base"/.[!.]* "$base"/..?*; do
    ((base_count += 1))
    [[ "${base_entry##*/}" == "$leaf_name" ]] ||
      fail "published native bundle contains a leaf not selected by the exact worker: $base_entry"
  done
  shopt -u nullglob
  (( base_count == 1 )) ||
    fail "published native bundle must contain exactly one selected leaf: $base"
  directory="$base/$leaf_name"
  validate_native_bundle_leaf "$directory"
}

validate_native_bundle_leaf() {
  local directory="$1"
  local entry
  local name
  local mode
  local count=0
  local cpu_count=0
  local total_bytes=0
  local size
  local required
  [[ -d "$directory" && ! -L "$directory" ]] ||
    fail "runtime generation has no exact native inference bundle leaf: $directory"
  mode="$(stat -c '%a' -- "$directory")"
  [[ "$mode" == 555 ]] || fail "native inference bundle directory is not immutable: $directory"
  [[ "$(stat -c '%u' -- "$directory")" == "$(id -u)" ]] ||
    fail "native inference bundle directory has the wrong owner: $directory"
  shopt -s nullglob
  for entry in "$directory"/* "$directory"/.[!.]* "$directory"/..?*; do
    name="${entry##*/}"
    [[ -f "$entry" && ! -L "$entry" && "$(stat -c '%h' -- "$entry")" == 1 ]] ||
      fail "native inference bundle entry is not an exact single-link regular file: $entry"
    case "$name" in
      libllama-common.so.0 | libmtmd.so.0 | libllama.so.0 | libggml.so.0 | libggml-base.so.0 | libggml-vulkan.so) ;;
      libggml-cpu-*.so) cpu_count=$((cpu_count + 1)) ;;
      *) fail "native inference bundle contains an unexpected file: $entry" ;;
    esac
    mode="$(stat -c '%a' -- "$entry")"
    [[ "$mode" == 555 ]] || fail "native inference bundle file is not immutable: $entry"
    [[ "$(stat -c '%u' -- "$entry")" == "$(id -u)" ]] ||
      fail "native inference bundle file has the wrong owner: $entry"
    size="$(stat -c '%s' -- "$entry")"
    [[ "$size" =~ ^[0-9]+$ ]] || fail "native inference bundle file has an invalid size: $entry"
    (( size <= 1024 * 1024 * 1024 )) || fail "native inference bundle file exceeds its size bound: $entry"
    total_bytes=$((total_bytes + size))
    (( total_bytes <= 4 * 1024 * 1024 * 1024 )) ||
      fail "native inference bundle exceeds its aggregate size bound: $directory"
    count=$((count + 1))
    (( count <= 64 )) || fail "native inference bundle exceeds its file bound: $directory"
  done
  shopt -u nullglob
  (( cpu_count > 0 )) || fail "native inference bundle has no CPU backend: $directory"
  for required in libllama-common.so.0 libmtmd.so.0 libllama.so.0 libggml.so.0 libggml-base.so.0; do
    [[ -f "$directory/$required" && ! -L "$directory/$required" ]] ||
      fail "native inference bundle is missing $required: $directory"
  done
}

collect_nix_runtime_references() {
  local generation="$1"
  local output_name="$2"
  local -n output="$output_name"
  local object
  local metadata
  local reference
  local -a objects=(
    "$generation/agl"
    "$generation/agl-process-launcher"
    "$generation/agl-inference-worker"
  )
  shopt -s nullglob
  objects+=("$generation/agl-inference-native"/*/*)
  shopt -u nullglob
  output=()
  for object in "${objects[@]}"; do
    if ! LC_ALL=C readelf -h "$object" >/dev/null 2>&1; then
      continue
    fi
    metadata="$(
      LC_ALL=C readelf -d "$object"
      LC_ALL=C readelf -l "$object"
    )" || fail "failed to inspect ELF runtime references: $object"
    while IFS= read -r reference; do
      [[ -n "$reference" ]] && output+=("$reference")
    done < <(
      grep -oE '/nix/store/[0-9a-z]{32}-[A-Za-z0-9+._?=-]+' <<<"$metadata" || true
    )
  done
  if (( ${#output[@]} > 0 )); then
    mapfile -t output < <(printf '%s\n' "${output[@]}" | sort -u)
  fi
  return 0
}

validate_nix_runtime_roots() {
  local generation="$1"
  local root_directory="$generation/.nix-gc-roots"
  local entry
  local name
  local remaining
  local target
  local -a references=()
  local -A expected=()
  collect_nix_runtime_references "$generation" references
  if (( ${#references[@]} == 0 )); then
    [[ ! -e "$root_directory" && ! -L "$root_directory" ]] ||
      fail "non-Nix runtime generation contains unexpected GC roots: $generation"
    return 0
  fi
  [[ -d "$root_directory" && ! -L "$root_directory" &&
    "$(stat -c '%a' -- "$root_directory")" == 555 &&
    "$(stat -c '%u' -- "$root_directory")" == "$(id -u)" ]] ||
    fail "Nix runtime generation has no sealed GC-root directory: $generation"
  for target in "${references[@]}"; do
    expected["$(basename -- "$target")"]="$target"
  done
  remaining=${#references[@]}
  shopt -s nullglob
  for entry in "$root_directory"/* "$root_directory"/.[!.]* "$root_directory"/..?*; do
    name="${entry##*/}"
    target="${expected[$name]:-}"
    [[ -n "$target" && -L "$entry" && "$(readlink -- "$entry")" == "$target" &&
      "$(readlink -f -- "$entry" 2>/dev/null || true)" == "$target" ]] ||
      fail "Nix runtime generation contains an invalid GC root: $entry"
    unset 'expected[$name]'
    remaining=$((remaining - 1))
  done
  shopt -u nullglob
  (( remaining == 0 )) ||
    fail "Nix runtime generation is missing an exact GC root: $generation"
  return 0
}

pin_nix_runtime_references() {
  local generation="$1"
  local root_directory="$generation/.nix-gc-roots"
  local reference
  local -a references=()
  collect_nix_runtime_references "$generation" references
  if (( ${#references[@]} == 0 )); then
    return 0
  fi
  command -v nix-store >/dev/null 2>&1 ||
    fail "Nix-linked runtime bundle requires nix-store to pin its external closure"
  [[ -d "$root_directory" && ! -L "$root_directory" ]] ||
    fail "Nix GC-root staging directory disappeared: $root_directory"
  for reference in "${references[@]}"; do
    [[ -e "$reference" ]] || fail "Nix runtime reference is already unavailable: $reference"
    nix-store \
      --add-root "$root_directory/$(basename -- "$reference")" \
      --realise "$reference" >/dev/null
  done
  chmod 0555 "$root_directory"
  sync_path "$root_directory"
  sync_path "$generation"
  validate_nix_runtime_roots "$generation"
}

resolve_current_generation() {
  local resolved
  [[ -L "$current_link" ]] || fail "runtime current pointer is missing: $current_link"
  resolved="$(readlink -f -- "$current_link")" || fail "runtime current pointer is broken: $current_link"
  [[ "$(dirname -- "$resolved")" == "$generations_dir" ]] ||
    fail "runtime current pointer escapes the generations directory: $current_link"
  reject_obsolete_two_binary_generation "$resolved"
  validate_generation "$resolved"
  validate_nix_runtime_roots "$resolved"
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
  validate_nix_runtime_roots "$generation"
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
run cargo "${worker_install_args[@]}"
run cargo "${agl_install_args[@]}"

native_bundle_relative="$(
  resolve_native_bundle_relative "$stage_root/bin/agl-inference-worker"
)"
native_bundle_leaf_name="${native_bundle_relative#agl-inference-native/}"
native_bundle_build_leaf="$cargo_target_dir/$cargo_profile/$native_bundle_relative"
validate_native_bundle_leaf "$native_bundle_build_leaf"
mkdir -p -- "$stage_root/bin/agl-inference-native"
cp -a -- \
  "$native_bundle_build_leaf" \
  "$stage_root/bin/agl-inference-native/$native_bundle_leaf_name"
# Keep the private transaction directory owner-mutable until its final atomic
# rename. The worker accepts owner-only staging authority, while the published
# generation is sealed to 0555 below.
chmod 0755 "$stage_root/bin/agl-inference-native"

[[ -x "$stage_root/bin/agl" && ! -L "$stage_root/bin/agl" ]] ||
  fail "staged cargo install did not produce a regular executable agl"
[[ -x "$stage_root/bin/agl-process-launcher" && ! -L "$stage_root/bin/agl-process-launcher" ]] ||
  fail "staged cargo install did not produce a regular executable process launcher"
[[ -x "$stage_root/bin/agl-inference-worker" && ! -L "$stage_root/bin/agl-inference-worker" ]] ||
  fail "staged cargo install did not produce a regular executable inference worker"
run "$stage_root/bin/agl" --version
run env AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE=1 "$stage_root/bin/agl"

# Cargo profile outputs and cargo-install staging files may be hard-linked on
# the same filesystem. Publish fresh inodes so no writable build-tree alias can
# mutate an allegedly immutable runtime executable after installation.
cp -- "$stage_root/bin/agl" "$generation_staging/agl"
cp -- "$stage_root/bin/agl-process-launcher" "$generation_staging/agl-process-launcher"
cp -- "$stage_root/bin/agl-inference-worker" "$generation_staging/agl-inference-worker"
mv -- "$stage_root/bin/agl-inference-native" "$generation_staging/agl-inference-native"
rm -rf -- "$stage_root"
chmod 0555 "$generation_staging/agl-inference-native"
chmod 0555 \
  "$generation_staging/agl" \
  "$generation_staging/agl-process-launcher" \
  "$generation_staging/agl-inference-worker"
generation_nix_references=()
collect_nix_runtime_references "$generation_staging" generation_nix_references
if (( ${#generation_nix_references[@]} > 0 )); then
  mkdir "$generation_staging/.nix-gc-roots"
  chmod 0755 "$generation_staging/.nix-gc-roots"
fi
chmod 0555 "$generation_staging"
validate_generation "$generation_staging"
sync_path "$generation_staging"
mv -- "$generation_staging" "$new_generation"
generation_staging=""
unpublished_generation="$new_generation"
pin_nix_runtime_references "$new_generation"
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
  unpublished_generation=""
  fault_inject after-initial-current-publish
else
  fault_inject before-new-current-publish
  publish_current "$new_generation"
  unpublished_generation=""
  fault_inject after-new-current-publish
fi

resolved_agl="$(readlink -f -- "$installed_agl")"
resolved_launcher="$(readlink -f -- "$installed_launcher")"
resolved_worker="$(readlink -f -- "$new_generation/agl-inference-worker")"
[[ "$resolved_agl" == "$new_generation/agl" ]] ||
  fail "installed agl did not resolve through the published generation"
[[ "$resolved_launcher" == "$new_generation/agl-process-launcher" ]] ||
  fail "installed launcher did not resolve through the published generation"
[[ "$resolved_worker" == "$new_generation/agl-inference-worker" ]] ||
  fail "installed inference worker did not remain private to the published generation"
[[ ! -e "$forbidden_public_worker" && ! -L "$forbidden_public_worker" ]] ||
  fail "refusing a public inference worker command under $install_bin"

echo "installed agl: $installed_agl -> $resolved_agl"
echo "installed process launcher: $installed_launcher -> $resolved_launcher"
echo "installed private inference worker: $resolved_worker"
run "$installed_agl" --version

systemd_user_dir="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}/systemd/user"
if [[ -f "$systemd_user_dir/agentlibre-daemon.socket" ]]; then
  echo "A preserved agentLIBRE user socket may still be stopped after a clean uninstall."
  echo "Start it with: systemctl --user start agentlibre-daemon.socket"
fi

trap - EXIT HUP INT TERM
if [[ ":$PATH:" != *":$install_bin:"* ]]; then
  echo "The install directory is not on PATH." >&2
  echo "Add this to your shell profile:" >&2
  echo "  export PATH=\"$install_bin:\$PATH\"" >&2
fi
