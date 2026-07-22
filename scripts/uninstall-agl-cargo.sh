#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/uninstall-agl-cargo.sh [options]

Validates and previews removal of an agentLIBRE runtime bundle installed by
scripts/install-agl-cargo.sh. Nothing is removed unless --apply is supplied.

Options:
  --root PATH   Uninstall below PATH instead of the resolved explicit root.
  --apply       Remove the validated managed runtime bundle.
  -h, --help    Show this help.
USAGE
}

fail() {
  echo "$*" >&2
  exit 1
}

apply=0
cargo_root=""
invocation_dir="$PWD"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      [[ $# -ge 2 ]] || fail "--root requires a path"
      cargo_root="$2"
      shift 2
      ;;
    --apply)
      apply=1
      shift
      ;;
    -h | --help)
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

[[ "$(uname -s)" == Linux ]] || fail "agentLIBRE runtime uninstall is Linux-only"

if [[ -z "$cargo_root" ]]; then
  cargo_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-${HOME:?HOME is required}/.cargo}}"
fi
if [[ "$cargo_root" != /* ]]; then
  cargo_root="$invocation_dir/$cargo_root"
fi
cargo_root="$(realpath -m -s -- "$cargo_root")"
resolved_cargo_root="$(realpath -m -- "$cargo_root")"
[[ "$resolved_cargo_root" == "$cargo_root" ]] ||
  fail "refusing an uninstall root whose path traverses a symlink: $cargo_root -> $resolved_cargo_root"

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
current_uid="$(id -u)"

if [[ ! -e "$runtime_dir" && ! -L "$runtime_dir" &&
  ! -e "$installed_agl" && ! -L "$installed_agl" &&
  ! -e "$installed_launcher" && ! -L "$installed_launcher" &&
  ! -e "$forbidden_public_worker" && ! -L "$forbidden_public_worker" &&
  ! -e "$runtime_lock" && ! -L "$runtime_lock" ]]; then
  echo "no managed agentLIBRE runtime bundle is installed under $cargo_root"
  exit 0
fi

require_managed_directory() {
  local path="$1"
  local label="$2"
  local mode
  [[ -d "$path" && ! -L "$path" ]] || fail "$label is not a real directory: $path"
  [[ "$(realpath -e -- "$path" 2>/dev/null || true)" == "$path" ]] ||
    fail "$label path is not canonical: $path"
  [[ "$(stat -c '%u' -- "$path")" == "$current_uid" ]] ||
    fail "$label is not owned by the current UID: $path"
  mode="$(stat -c '%a' -- "$path")"
  (( (8#$mode & 0022) == 0 )) ||
    fail "$label is group/other-writable: $path (mode $mode)"
}

require_exact_executable() {
  local path="$1"
  local label="$2"
  [[ -f "$path" && -x "$path" && ! -L "$path" &&
    "$(stat -c '%a' -- "$path")" == 555 &&
    "$(stat -c '%u' -- "$path")" == "$current_uid" &&
    "$(stat -c '%h' -- "$path")" == 1 ]] ||
    fail "$label is not an immutable owned single-link executable: $path"
}

validate_native_bundle() {
  local base="$1"
  local entry
  local leaf=""
  local name
  local cpu_count=0
  local entry_count=0
  local -A required=(
    [libllama-common.so.0]=0
    [libmtmd.so.0]=0
    [libllama.so.0]=0
    [libggml.so.0]=0
    [libggml-base.so.0]=0
  )
  [[ -d "$base" && ! -L "$base" &&
    "$(stat -c '%a' -- "$base")" == 555 &&
    "$(stat -c '%u' -- "$base")" == "$current_uid" ]] ||
    fail "native inference bundle base is not exact: $base"
  shopt -s nullglob
  for entry in "$base"/* "$base"/.[!.]* "$base"/..?*; do
    [[ -z "$leaf" ]] || fail "native inference bundle has multiple leaves: $base"
    leaf="$entry"
  done
  shopt -u nullglob
  [[ -n "$leaf" && -d "$leaf" && ! -L "$leaf" &&
    "${leaf##*/}" =~ ^sha256-[0-9a-f]{64}$ &&
    "$(stat -c '%a' -- "$leaf")" == 555 &&
    "$(stat -c '%u' -- "$leaf")" == "$current_uid" ]] ||
    fail "native inference bundle leaf is not exact: $base"
  shopt -s nullglob
  for entry in "$leaf"/* "$leaf"/.[!.]* "$leaf"/..?*; do
    name="${entry##*/}"
    [[ -f "$entry" && ! -L "$entry" &&
      "$(stat -c '%a' -- "$entry")" == 555 &&
      "$(stat -c '%u' -- "$entry")" == "$current_uid" &&
      "$(stat -c '%h' -- "$entry")" == 1 ]] ||
      fail "native inference bundle entry is not exact: $entry"
    case "$name" in
      libllama-common.so.0 | libmtmd.so.0 | libllama.so.0 | libggml.so.0 | libggml-base.so.0)
        required["$name"]=1
        ;;
      libggml-cpu-*.so) cpu_count=$((cpu_count + 1)) ;;
      libggml-vulkan.so) ;;
      *) fail "native inference bundle contains an unexpected entry: $entry" ;;
    esac
    entry_count=$((entry_count + 1))
    (( entry_count <= 64 )) || fail "native inference bundle exceeds its file bound: $leaf"
  done
  shopt -u nullglob
  (( cpu_count > 0 )) || fail "native inference bundle has no CPU backend: $leaf"
  for name in "${!required[@]}"; do
    [[ "${required[$name]}" == 1 ]] ||
      fail "native inference bundle is missing $name: $leaf"
  done
}

validate_nix_gc_roots() {
  local directory="$1"
  local entry
  local entry_count=0
  local target
  [[ -d "$directory" && ! -L "$directory" &&
    "$(stat -c '%a' -- "$directory")" == 555 &&
    "$(stat -c '%u' -- "$directory")" == "$current_uid" ]] ||
    fail "Nix GC-root directory is not exact: $directory"
  shopt -s nullglob
  for entry in "$directory"/* "$directory"/.[!.]* "$directory"/..?*; do
    [[ -L "$entry" ]] || fail "Nix GC-root entry is not a symlink: $entry"
    target="$(readlink -- "$entry")"
    [[ "$target" == /nix/store/* && "$(basename -- "$target")" == "${entry##*/}" &&
      "$(readlink -f -- "$entry" 2>/dev/null || true)" == "$target" ]] ||
      fail "Nix GC-root entry has an unexpected target: $entry"
    entry_count=$((entry_count + 1))
  done
  shopt -u nullglob
  (( entry_count > 0 )) || fail "Nix GC-root directory is empty: $directory"
}

validate_generation() {
  local generation="$1"
  local entry
  local name
  local entry_count=0
  local has_worker=0
  local has_native=0
  local has_gc_roots=0
  [[ -d "$generation" && ! -L "$generation" &&
    "$(stat -c '%a' -- "$generation")" == 555 &&
    "$(stat -c '%u' -- "$generation")" == "$current_uid" ]] ||
    fail "runtime generation is not an immutable owned directory: $generation"
  [[ "${generation##*/}" =~ ^generation-[A-Za-z0-9]+$ ]] ||
    fail "runtime generation has an unexpected name: $generation"
  shopt -s nullglob
  for entry in "$generation"/* "$generation"/.[!.]* "$generation"/..?*; do
    name="${entry##*/}"
    case "$name" in
      agl | agl-process-launcher) ;;
      agl-inference-worker) has_worker=1 ;;
      agl-inference-native) has_native=1 ;;
      .nix-gc-roots) has_gc_roots=1 ;;
      *)
        shopt -u nullglob
        fail "runtime generation contains an unexpected entry: $entry"
        ;;
    esac
    entry_count=$((entry_count + 1))
  done
  shopt -u nullglob
  require_exact_executable "$generation/agl" "agl"
  require_exact_executable "$generation/agl-process-launcher" "process launcher"
  if (( entry_count == 2 && has_worker == 0 && has_native == 0 && has_gc_roots == 0 )); then
    printf 'obsolete-two-binary\n'
    return 0
  fi
  (( has_worker == 1 && has_native == 1 && (entry_count == 4 || entry_count == 5) )) ||
    fail "runtime generation is neither an exact current nor obsolete alpha layout: $generation"
  if (( (entry_count == 5) != (has_gc_roots == 1) )); then
    fail "runtime generation has an inconsistent Nix GC-root layout: $generation"
  fi
  require_exact_executable "$generation/agl-inference-worker" "inference worker"
  validate_native_bundle "$generation/agl-inference-native"
  if (( has_gc_roots == 1 )); then
    validate_nix_gc_roots "$generation/.nix-gc-roots"
  fi
  printf 'current\n'
}

require_managed_directory "$cargo_root" "install root"
require_managed_directory "$install_bin" "install bin directory"
require_managed_directory "$cargo_root/libexec" "libexec directory"
require_managed_directory "$runtime_dir" "runtime directory"
require_managed_directory "$generations_dir" "runtime generations directory"
[[ -f "$runtime_lock" && ! -L "$runtime_lock" &&
  "$(stat -c '%a' -- "$runtime_lock")" == 600 &&
  "$(stat -c '%u' -- "$runtime_lock")" == "$current_uid" &&
  "$(stat -c '%h' -- "$runtime_lock")" == 1 ]] ||
  fail "runtime lock is not an exact private owned file: $runtime_lock"
command -v flock >/dev/null 2>&1 || fail "missing required tool: flock"
if (( apply == 1 )); then
  exec {runtime_lock_fd}<>"$runtime_lock"
  flock_mode="--exclusive"
else
  exec {runtime_lock_fd}<"$runtime_lock"
  flock_mode="--shared"
fi
if ! flock "$flock_mode" --nonblock "$runtime_lock_fd"; then
  fail "refusing to uninstall while another runtime bundle operation holds: $runtime_lock"
fi

[[ ! -e "$forbidden_public_worker" && ! -L "$forbidden_public_worker" ]] ||
  fail "refusing to remove a surface with a public inference worker: $forbidden_public_worker"
[[ -L "$installed_agl" && "$(readlink -- "$installed_agl")" == "$agl_link_target" ]] ||
  fail "agl is not an exact managed link: $installed_agl"
[[ -L "$installed_launcher" && "$(readlink -- "$installed_launcher")" == "$launcher_link_target" ]] ||
  fail "process launcher is not an exact managed link: $installed_launcher"
[[ -L "$current_link" ]] || fail "runtime current pointer is not a symlink: $current_link"
current_target="$(readlink -- "$current_link")"
[[ "$current_target" =~ ^generations/(generation-[A-Za-z0-9]+)$ ]] ||
  fail "runtime current pointer has an unexpected target: $current_link -> $current_target"
current_generation="$(readlink -f -- "$current_link" 2>/dev/null || true)"
[[ -n "$current_generation" && "$(dirname -- "$current_generation")" == "$generations_dir" ]] ||
  fail "runtime current pointer is broken or escapes the generations directory: $current_link"

shopt -s nullglob
runtime_entries=("$runtime_dir"/* "$runtime_dir"/.[!.]* "$runtime_dir"/..?*)
shopt -u nullglob
(( ${#runtime_entries[@]} == 2 )) || fail "runtime directory contains unexpected entries: $runtime_dir"
for runtime_entry in "${runtime_entries[@]}"; do
  case "${runtime_entry##*/}" in
    current | generations) ;;
    *) fail "runtime directory contains an unexpected entry: $runtime_entry" ;;
  esac
done

generation_paths=()
obsolete_count=0
current_count=0
shopt -s nullglob
for generation in "$generations_dir"/* "$generations_dir"/.[!.]* "$generations_dir"/..?*; do
  layout="$(validate_generation "$generation")"
  generation_paths+=("$generation")
  case "$layout" in
    obsolete-two-binary) obsolete_count=$((obsolete_count + 1)) ;;
    current) current_count=$((current_count + 1)) ;;
    *) fail "runtime generation returned an unknown layout: $generation" ;;
  esac
done
shopt -u nullglob
(( ${#generation_paths[@]} > 0 )) || fail "runtime generations directory is empty: $generations_dir"
current_seen=0
for generation in "${generation_paths[@]}"; do
  [[ "$generation" != "$current_generation" ]] || current_seen=1
done
(( current_seen == 1 )) || fail "runtime current pointer does not select a managed generation"
[[ "$(readlink -f -- "$installed_agl" 2>/dev/null || true)" == "$current_generation/agl" ]] ||
  fail "managed agl link does not resolve through current: $installed_agl"
[[ "$(readlink -f -- "$installed_launcher" 2>/dev/null || true)" == "$current_generation/agl-process-launcher" ]] ||
  fail "managed launcher link does not resolve through current: $installed_launcher"

declare -A managed_executable_inodes=()
for generation in "${generation_paths[@]}"; do
  for executable_name in agl agl-process-launcher agl-inference-worker; do
    executable="$generation/$executable_name"
    if [[ -f "$executable" && ! -L "$executable" ]]; then
      managed_executable_inodes["$(stat -Lc '%d:%i' -- "$executable")"]=1
    fi
  done
done

scan_runtime_processes() {
  local output_name="$1"
  local -n output="$output_name"
  local process_exe
  local process_inode
  local process_pid
  local process_target
  shopt -s nullglob
  for process_exe in /proc/[0-9]*/exe; do
    process_inode="$(stat -Lc '%d:%i' -- "$process_exe" 2>/dev/null || true)"
    [[ -n "$process_inode" && -n "${managed_executable_inodes[$process_inode]:-}" ]] || continue
    process_pid="${process_exe#/proc/}"
    process_pid="${process_pid%/exe}"
    process_target="$(readlink -- "$process_exe" 2>/dev/null || true)"
    output+=("running process uses the runtime bundle: pid=$process_pid executable=$process_target")
  done
  shopt -u nullglob
}

block_reasons=()
if command -v systemctl >/dev/null 2>&1; then
  for unit in agentlibre-daemon.service agentlibre-daemon.socket; do
    if systemctl --user is-active --quiet "$unit" >/dev/null 2>&1; then
      block_reasons+=("active systemd user unit: $unit")
    fi
  done
fi
scan_runtime_processes block_reasons

echo "agentLIBRE managed runtime uninstall plan"
echo "  root: $cargo_root"
echo "  current generation: $current_generation"
echo "  generations: ${#generation_paths[@]} (current=$current_count obsolete=$obsolete_count)"
echo "  remove: $installed_agl"
echo "  remove: $installed_launcher"
echo "  remove: $current_link"
for generation in "${generation_paths[@]}"; do
  echo "  remove: $generation"
done
echo "  remove: $generations_dir"
echo "  remove: $runtime_dir"
echo "  remove: $runtime_lock"
echo "  preserve: user configuration, state, models, and systemd unit files"
if (( ${#block_reasons[@]} > 0 )); then
  for reason in "${block_reasons[@]}"; do
    echo "  blocked: $reason"
  done
  echo "  stop standard units with: systemctl --user stop agentlibre-daemon.socket agentlibre-daemon.service"
  echo "  stop the listed process or unit before applying this plan"
fi

if (( apply == 0 )); then
  echo "preview only; rerun with --apply to remove this exact managed bundle"
  exit 0
fi
(( ${#block_reasons[@]} == 0 )) || fail "refusing to uninstall a runtime bundle that is active"

rm -f -- "$installed_agl" "$installed_launcher" "$current_link"
late_block_reasons=()
scan_runtime_processes late_block_reasons
if (( ${#late_block_reasons[@]} > 0 )); then
  ln -s -- "$current_target" "$current_link"
  ln -s -- "$agl_link_target" "$installed_agl"
  ln -s -- "$launcher_link_target" "$installed_launcher"
  for reason in "${late_block_reasons[@]}"; do
    echo "blocked after detaching public links: $reason" >&2
  done
  fail "runtime became active during uninstall; restored the managed links"
fi
for generation in "${generation_paths[@]}"; do
  find -P "$generation" -type d -exec chmod u+rwx -- {} +
  rm -rf -- "$generation"
done
rmdir -- "$generations_dir"
rmdir -- "$runtime_dir"
rm -f -- "$runtime_lock"

echo "removed managed agentLIBRE runtime bundle from $cargo_root"
