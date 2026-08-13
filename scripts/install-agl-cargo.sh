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
  --terminal-generation DIR
                       Pair this agent generation with one exact immutable
                       terminal generation directory (required).
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
terminal_generation=""
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
    --terminal-generation)
      [[ $# -ge 2 ]] || fail "--terminal-generation requires a directory"
      terminal_generation="$2"
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

[[ -n "$terminal_generation" ]] || fail "--terminal-generation DIR is required"
[[ "$terminal_generation" == /* ]] || terminal_generation="$invocation_dir/$terminal_generation"
terminal_generation="$(realpath -m -s -- "$terminal_generation")"
[[ ! -L "$terminal_generation" ]] || fail "terminal generation input must not be a symlink"
resolved_terminal_generation="$(realpath -e -- "$terminal_generation" 2>/dev/null || true)"
[[ -n "$resolved_terminal_generation" && "$resolved_terminal_generation" == "$terminal_generation" ]] ||
  fail "terminal generation must be one canonical existing directory: $terminal_generation"
[[ -d "$terminal_generation" && ! -L "$terminal_generation" &&
  "$(stat -c '%a' -- "$terminal_generation")" == 555 &&
  "$(stat -c '%u' -- "$terminal_generation")" == "$(id -u)" ]] ||
  fail "terminal generation must be an immutable current-user directory: $terminal_generation"
terminal_manifest="$terminal_generation/runtime-manifest.json"
[[ -f "$terminal_manifest" && ! -L "$terminal_manifest" &&
  "$(stat -c '%a' -- "$terminal_manifest")" == 444 &&
  "$(stat -c '%h' -- "$terminal_manifest")" == 1 ]] ||
  fail "terminal generation has no sealed runtime-manifest.json"
grep -F '"schema":"agl-terminal.runtime-generation.v2"' "$terminal_manifest" >/dev/null ||
  grep -F '"schema": "agl-terminal.runtime-generation.v2"' "$terminal_manifest" >/dev/null ||
  fail "terminal generation manifest is not v2"
terminal_revision="$(sed -n 's/.*"source_revision"[[:space:]]*:[[:space:]]*"\([0-9a-f]\{40\}\)".*/\1/p' "$terminal_manifest" | head -n1)"
[[ -n "$terminal_revision" ]] || fail "terminal generation manifest has no canonical source revision"
agent_source_revision="$(git -C "$repo_root" rev-parse --verify HEAD^{commit})" ||
  fail "failed to resolve agent source revision"
[[ "$terminal_revision" == "$agent_source_revision" ]] ||
  fail "terminal generation source revision does not match this agent build source"
for terminal_entry in agl-terminald agl-process-launcher agl-terminal; do
  terminal_path="$terminal_generation/$terminal_entry"
  [[ -f "$terminal_path" && -x "$terminal_path" && ! -L "$terminal_path" &&
    "$(stat -c '%a' -- "$terminal_path")" == 555 &&
    "$(stat -c '%h' -- "$terminal_path")" == 1 ]] ||
    fail "terminal generation entry is not immutable: $terminal_path"
  terminal_digest="sha256:$(sha256sum -- "$terminal_path" | awk '{print $1}')"
  grep -F "\"sha256\":\"$terminal_digest\"" "$terminal_manifest" >/dev/null ||
    grep -F "\"sha256\": \"$terminal_digest\"" "$terminal_manifest" >/dev/null ||
    fail "terminal generation entry digest is not sealed: $terminal_entry"
done
terminal_manifest_digest="sha256:$(sha256sum -- "$terminal_manifest" | awk '{print $1}')"
[[ "$(basename -- "$terminal_generation")" == "generation-${terminal_manifest_digest#sha256:}" ]] ||
  fail "terminal generation directory does not match its full manifest digest"
[[ "$(find "$terminal_generation" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort)" == $'agl-process-launcher\nagl-terminal\nagl-terminald\nruntime-manifest.json' ]] ||
  fail "terminal generation inventory is not canonical"
terminal_product_root="$(dirname -- "$(dirname -- "$terminal_generation")")"
terminal_generations_root="$(dirname -- "$terminal_generation")"
for terminal_ancestor in "$terminal_product_root" "$terminal_generations_root"; do
  terminal_ancestor_mode="$(stat -c '%a' -- "$terminal_ancestor")"
  [[ -d "$terminal_ancestor" && ! -L "$terminal_ancestor" &&
    "$(realpath -e -- "$terminal_ancestor")" == "$terminal_ancestor" &&
    "$(stat -c '%u' -- "$terminal_ancestor")" == "$(id -u)" ]] ||
    fail "terminal generation has an unsafe managed ancestor: $terminal_ancestor"
  (( (8#$terminal_ancestor_mode & 0022) == 0 )) ||
    fail "terminal generation ancestor is group/other writable: $terminal_ancestor"
done
terminal_operation_lock="$terminal_product_root/.operation.lock"

require_clean_agent_source() {
  [[ "$(git -C "$repo_root" rev-parse --verify HEAD^{commit})" == "$agent_source_revision" ]] ||
    fail "agent source revision changed during installation"
  [[ -z "$(git -C "$repo_root" status --porcelain=v1 --untracked-files=normal --ignore-submodules=none)" ]] ||
    fail "agent installation requires one clean root workspace"
}

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
forbidden_public_worker="$install_bin/agl-inference-worker"
forbidden_public_launcher="$install_bin/agl-process-launcher"
agl_link_target="../libexec/agentlibre/current/agl"

cd "$repo_root"

if [[ "$dry_run" -eq 0 ]]; then
  need_tool cargo
  need_tool flock
  need_tool git
  need_tool readelf
  need_tool sync
  require_clean_agent_source
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
  if [[ ! -x "$llama_lib_dir/llama-server" ]]; then
    missing_llama_lib=1
  fi
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
set_install_args() {
  local stage_root="$1"
  agl_install_args=(
    install
    --path "$repo_root/crates/agl-cli"
    --bin agl
    "${install_options[@]}"
    --root "$stage_root"
  )
}

if [[ "$dry_run" -eq 1 ]]; then
  printf '+ verify exact terminal generation %s offline\n' "$terminal_generation"
  printf '+ hold terminal operation lock %q through agent generation sealing\n' "$terminal_operation_lock"
  dry_stage_root="$generations_dir/.staging.DRY-RUN/.cargo-root"
  set_install_args "$dry_stage_root"
  run cargo "${agl_install_args[@]}"
  printf '+ copy private llama-server and exact lib*.so* closure from %q into runtime generation\n' \
    "${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}/bin"
  printf '+ seal terminal manifest digest into agent runtime-manifest v3: %s\n' "$terminal_manifest_digest"
  printf '+ validate sealed runtime identity before and after atomic generation publication\n'
  printf '+ pin exact Nix runtime references below final generation .nix-gc-roots\n'
  printf '+ publish complete generation through %q\n' "$current_link"
  printf '+ reconcile an exact preserved agentlibre-daemon.service/socket pair after publication\n'
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

if [[ -L "$terminal_operation_lock" || ( -e "$terminal_operation_lock" && ! -f "$terminal_operation_lock" ) ]]; then
  fail "refusing non-regular terminal operation lock: $terminal_operation_lock"
fi
if [[ ! -e "$terminal_operation_lock" ]]; then
  (umask 077 && : >>"$terminal_operation_lock")
fi
exec {terminal_operation_lock_fd}<>"$terminal_operation_lock"
if ! flock --exclusive --nonblock "$terminal_operation_lock_fd"; then
  fail "terminal generation is busy with another install/uninstall operation"
fi

if [[ -e "$forbidden_public_worker" || -L "$forbidden_public_worker" ]]; then
  fail "refusing a public inference worker command; remove it before installing the private runtime bundle: $forbidden_public_worker"
fi
if [[ -e "$forbidden_public_launcher" || -L "$forbidden_public_launcher" ]]; then
  fail "refusing the obsolete terminal-owned launcher surface; uninstall the old combined alpha runtime first: $forbidden_public_launcher"
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

if [[ "$force" -eq 0 && ( -e "$installed_agl" || -L "$installed_agl" ) ]]; then
  fail "refusing to replace the existing runtime bundle because --no-force was requested"
fi

for install_artifact in "$installed_agl" "$current_link"; do
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
  local entry
  local executable
  local name
  local entry_count=0
  local has_gc_roots=0
  [[ -d "$generation" && ! -L "$generation" &&
    "$(stat -c '%a' -- "$generation")" == 555 &&
    "$(stat -c '%u' -- "$generation")" == "$(id -u)" ]] ||
    fail "runtime generation is not an exact immutable directory: $generation"
  [[ -f "$generation/agl" && -x "$generation/agl" && ! -L "$generation/agl" ]] ||
    fail "runtime generation has no regular executable agl: $generation"
  [[ -f "$generation/llama-server" && -x "$generation/llama-server" && ! -L "$generation/llama-server" ]] ||
    fail "runtime generation has no regular private llama-server: $generation"
  for executable in \
    "$generation/agl" \
    "$generation/llama-server"
  do
    [[ "$(stat -c '%a' -- "$executable")" == 555 &&
      "$(stat -c '%u' -- "$executable")" == "$(id -u)" &&
      "$(stat -c '%h' -- "$executable")" == 1 ]] ||
      fail "runtime generation executable is not an exact immutable single-link file: $executable"
  done
  [[ -f "$generation/runtime-manifest.json" && ! -L "$generation/runtime-manifest.json" &&
    "$(stat -c '%a' -- "$generation/runtime-manifest.json")" == 444 &&
    "$(stat -c '%u' -- "$generation/runtime-manifest.json")" == "$(id -u)" &&
    "$(stat -c '%h' -- "$generation/runtime-manifest.json")" == 1 ]] ||
    fail "runtime generation has no exact sealed manifest: $generation"
  shopt -s nullglob
  for entry in "$generation"/* "$generation"/.[!.]* "$generation"/..?*; do
    name="${entry##*/}"
    case "$name" in
      agl | llama-server | lib*.so | lib*.so.* | runtime-manifest.json) ;;
      .nix-gc-roots) has_gc_roots=1 ;;
      *)
        shopt -u nullglob
        fail "runtime generation contains an unexpected entry: $entry"
        ;;
    esac
    entry_count=$((entry_count + 1))
  done
  shopt -u nullglob
  (( entry_count >= 4 )) ||
    fail "runtime generation does not have the exact manifest-bearing layout: $generation"
  shopt -s nullglob
  local -a engine_libraries=("$generation"/lib*.so*)
  shopt -u nullglob
  (( ${#engine_libraries[@]} > 0 )) ||
    fail "runtime generation has no private llama-server library closure: $generation"
  for entry in "${engine_libraries[@]}"; do
    [[ -f "$entry" && ! -L "$entry" &&
      "$(stat -c '%a' -- "$entry")" == 555 &&
      "$(stat -c '%u' -- "$entry")" == "$(id -u)" &&
      "$(stat -c '%h' -- "$entry")" == 1 ]] ||
      fail "runtime engine library is not an exact immutable single-link file: $entry"
  done
  "$generation/agl" runtime identity >/dev/null ||
    fail "runtime generation manifest or component identity failed verification: $generation"
}

is_obsolete_combined_generation() {
  local generation="$1"
  [[ -d "$generation" && ! -L "$generation" &&
    "$(stat -c '%a' -- "$generation")" == 555 &&
    "$(stat -c '%u' -- "$generation")" == "$(id -u)" ]] ||
    return 1
  [[ -f "$generation/agl-process-launcher" &&
    ! -L "$generation/agl-process-launcher" ]]
}

reject_obsolete_combined_generation() {
  local generation="$1"
  is_obsolete_combined_generation "$generation" || return 0
  echo "existing managed runtime uses an obsolete combined agent/terminal alpha layout" >&2
  echo "agentLIBRE alpha installers do not migrate obsolete runtime generations" >&2
  echo "move or remove all of these managed artifacts before a clean install:" >&2
  echo "  $installed_agl" >&2
  echo "  $forbidden_public_launcher" >&2
  echo "  $current_link" >&2
  echo "  $generation" >&2
  echo "the manifest-aware uninstaller intentionally rejects this obsolete alpha layout" >&2
  fail "move or remove the listed obsolete artifacts, then rerun the installer"
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
    "$generation/llama-server"
  )
  shopt -s nullglob
  objects+=("$generation"/lib*.so*)
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
  reject_obsolete_combined_generation "$resolved"
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
  validate_generation "$generation"
  validate_nix_runtime_roots "$generation"
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

  agl_kind="$(managed_link_kind "$installed_agl" "$agl_link_target")"
  if [[ "$agl_kind" == regular ]]; then
    echo "refusing to replace an unmanaged regular runtime command under $install_bin" >&2
    echo "agentLIBRE alpha installs do not migrate flat binaries; move or remove" >&2
    echo "  $installed_agl" >&2
    fail "then rerun the installer"
  fi
  [[ "$agl_kind" != invalid ]] || fail "refusing to replace unmanaged agl artifact: $installed_agl"

  if [[ ! -e "$current_link" && ! -L "$current_link" ]]; then
    if [[ "$agl_kind" == absent || "$agl_kind" == managed ]]; then
      surface_mode="fresh"
      return 0
    fi
    fail "refusing an unsupported fresh-install surface under $install_bin"
  fi

  [[ -L "$current_link" ]] || fail "refusing to replace non-symlink current pointer: $current_link"
  resolve_current_generation >/dev/null
  if [[ "$agl_kind" == managed ]]; then
    surface_mode="update"
    return 0
  fi

  fail "refusing an incomplete managed runtime surface under $install_bin; restore the managed agl link before retrying"
}

surface_mode=""
classify_managed_surface

generation_staging="$(mktemp -d "$generations_dir/.staging.XXXXXXXX")"
generation_token="${generation_staging##*.staging.}"
new_generation="$generations_dir/generation-$generation_token"
stage_root="$generation_staging/.cargo-root"
mkdir -p "$stage_root"
set_install_args "$stage_root"

run cargo "${agl_install_args[@]}"
require_clean_agent_source

[[ -x "$stage_root/bin/agl" && ! -L "$stage_root/bin/agl" ]] ||
  fail "staged cargo install did not produce a regular executable agl"
llama_build_bin="${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}/bin"
[[ -x "$llama_build_bin/llama-server" && ! -L "$llama_build_bin/llama-server" ]] ||
  fail "llama.cpp build did not produce a regular llama-server"
run "$stage_root/bin/agl" --version

# Cargo profile outputs and cargo-install staging files may be hard-linked on
# the same filesystem. Publish fresh inodes so no writable build-tree alias can
# mutate an allegedly immutable runtime executable after installation.
cp -- "$stage_root/bin/agl" "$generation_staging/agl"
cp -- "$llama_build_bin/llama-server" "$generation_staging/llama-server"
shopt -s nullglob
llama_libraries=("$llama_build_bin"/lib*.so*)
shopt -u nullglob
(( ${#llama_libraries[@]} > 0 )) || fail "llama.cpp build has no shared-library closure"
for library in "${llama_libraries[@]}"; do
  cp -L -- "$library" "$generation_staging/${library##*/}"
done
rm -rf -- "$stage_root"
chmod 0555 \
  "$generation_staging/agl" \
  "$generation_staging/llama-server" \
  "$generation_staging"/lib*.so*

runtime_source_state="clean"
runtime_source_revision="$agent_source_revision"
runtime_source_tree="$(git -C "$repo_root" rev-parse --verify 'HEAD^{tree}')" ||
  fail "failed to resolve runtime source tree"
runtime_seal_environment=(
  "AGL_INTERNAL_SEAL_RUNTIME_MANIFEST=$generation_staging"
  "AGL_INTERNAL_RUNTIME_SOURCE_STATE=$runtime_source_state"
  "AGL_INTERNAL_TERMINAL_GENERATION=$terminal_generation"
)
runtime_seal_environment+=(
  "AGL_INTERNAL_RUNTIME_SOURCE_REVISION=$runtime_source_revision"
  "AGL_INTERNAL_RUNTIME_SOURCE_TREE=$runtime_source_tree"
)
run env "${runtime_seal_environment[@]}" "$generation_staging/agl"

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
resolved_engine="$(readlink -f -- "$new_generation/llama-server")"
[[ "$resolved_agl" == "$new_generation/agl" ]] ||
  fail "installed agl did not resolve through the published generation"
[[ "$resolved_engine" == "$new_generation/llama-server" ]] ||
  fail "installed llama-server did not remain private to the published generation"
[[ ! -e "$forbidden_public_worker" && ! -L "$forbidden_public_worker" ]] ||
  fail "refusing a public inference worker command under $install_bin"
[[ ! -e "$forbidden_public_launcher" && ! -L "$forbidden_public_launcher" ]] ||
  fail "refusing a terminal-owned launcher command under $install_bin"

echo "installed agl: $installed_agl -> $resolved_agl"
echo "installed private llama-server: $resolved_engine"
run "$installed_agl" --version
run "$installed_agl" runtime identity

systemd_user_dir="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}/systemd/user"
standard_service_unit="agentlibre-daemon.service"
standard_socket_unit="agentlibre-daemon.socket"
standard_service_file="$systemd_user_dir/$standard_service_unit"
standard_socket_file="$systemd_user_dir/$standard_socket_unit"

systemd_reconciliation_failed() {
  fail "runtime publication succeeded; systemd reconciliation failed: $*"
}

standard_unit_file_is_safe() {
  local path="$1"
  local mode
  [[ -f "$path" && ! -L "$path" && "$(stat -c '%u' -- "$path")" == "$(id -u)" ]] ||
    return 1
  mode="$(stat -c '%a' -- "$path")"
  (( (8#$mode & 0022) == 0 ))
}

loaded_unit_is_exact() {
  local unit="$1"
  local expected_fragment="$2"
  local expected_exec_prefix="${3:-}"
  local details
  local fragment=""
  local dropins=""
  local exec_start=""
  local key
  local value
  details="$(systemctl --user show "$unit" -p FragmentPath -p DropInPaths -p ExecStart 2>/dev/null)" ||
    return 10
  while IFS='=' read -r key value; do
    case "$key" in
      FragmentPath) fragment="$value" ;;
      DropInPaths) dropins="$value" ;;
      ExecStart) exec_start="$value" ;;
    esac
  done <<<"$details"
  [[ "$fragment" == "$expected_fragment" && -z "$dropins" ]] || return 11
  [[ -z "$expected_exec_prefix" || "$exec_start" == "$expected_exec_prefix"* ]] || return 11
}

if [[ -e "$standard_service_file" || -L "$standard_service_file" ||
      -e "$standard_socket_file" || -L "$standard_socket_file" ]]; then
  if ! standard_unit_file_is_safe "$standard_service_file" ||
    ! standard_unit_file_is_safe "$standard_socket_file" ||
    ! grep -Fqx "Requires=$standard_socket_unit" "$standard_service_file" ||
    ! grep -Fqx "Service=$standard_service_unit" "$standard_socket_file" ||
    ! grep -Fqx "Accept=no" "$standard_socket_file"; then
    echo "preserved agentLIBRE user units are customized; leaving their lifecycle unchanged"
  elif ! command -v systemctl >/dev/null 2>&1; then
    systemd_reconciliation_failed "systemctl is unavailable for the preserved standard unit pair"
  else
    expected_service_exec="{ path=$installed_agl ; argv[]=$installed_agl serve --systemd-activation "
    loaded_pair_status=0
    loaded_unit_is_exact "$standard_service_unit" "$standard_service_file" \
      "$expected_service_exec" || loaded_pair_status=$?
    if (( loaded_pair_status == 0 )); then
      loaded_unit_is_exact "$standard_socket_unit" "$standard_socket_file" || loaded_pair_status=$?
    fi
    (( loaded_pair_status != 10 )) ||
      systemd_reconciliation_failed "could not query the preserved standard unit pair"
    if (( loaded_pair_status != 0 )); then
      echo "preserved agentLIBRE user units are not the exact loaded standard pair; leaving their lifecycle unchanged"
    else
    systemctl --user daemon-reload ||
      systemd_reconciliation_failed "daemon-reload failed"
    loaded_unit_is_exact "$standard_service_unit" "$standard_service_file" \
      "$expected_service_exec" &&
      loaded_unit_is_exact "$standard_socket_unit" "$standard_socket_file" ||
      systemd_reconciliation_failed "the loaded standard unit pair changed during daemon-reload"
    systemctl --user reset-failed "$standard_service_unit" "$standard_socket_unit" ||
      systemd_reconciliation_failed "reset-failed failed for the standard unit pair"
    systemctl --user start "$standard_socket_unit" ||
      systemd_reconciliation_failed "could not start the standard daemon socket"
    systemctl --user try-restart "$standard_service_unit" ||
      systemd_reconciliation_failed "could not replace the active standard daemon service"
    systemctl --user is-active --quiet "$standard_socket_unit" ||
      systemd_reconciliation_failed "the standard daemon socket is not active after start"
    echo "reconciled standard agentLIBRE user units with the installed generation"
    fi
  fi
fi

trap - EXIT HUP INT TERM
if [[ ":$PATH:" != *":$install_bin:"* ]]; then
  echo "The install directory is not on PATH." >&2
  echo "Add this to your shell profile:" >&2
  echo "  export PATH=\"$install_bin:\$PATH\"" >&2
fi
