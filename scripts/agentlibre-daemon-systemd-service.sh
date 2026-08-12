#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/agentlibre-daemon-systemd-service.sh [OPTIONS]

Installs paired user-systemd socket and service units for `agl serve`.

If VK_DRIVER_FILES is present, its explicit Vulkan driver manifest selection is
captured in the service. VK_ICD_FILENAMES is accepted only when
VK_DRIVER_FILES is absent and is normalized to VK_DRIVER_FILES in the unit.

Options:
  --unit NAME           systemd user service unit name
  --cwd PATH            working directory for the service
  --binary PATH         installed managed agl runtime path or alias
  --function ID_OR_PATH Function with an authoritative embedded profile
  --socket PATH         daemon Unix socket path
  --workspace-root PATH workspace root passed to agl serve
  --max-output-tokens N max generated tokens per turn
  --tool-mode MODE      read-only, write, execute, approve, or admin
  --log-filter FILTER   tracing filter for AGL_LOG
  --enable              enable the socket unit
  --restart             restart the socket unit after writing it
  --dry-run             print both units without writing them
  -h, --help            show this help

Defaults:
  --unit              agentlibre-daemon.service
  --cwd               current git repo root, or current directory outside git
  --binary            installed agl resolved from PATH
  --function          unset; use the workspace default Function
  --socket            ~/.local/state/agentLIBRE/daemon/agl.sock
  --workspace-root    repo root
  --max-output-tokens 256
  --tool-mode         read-only
  --log-filter        agentlibre=info,warn
EOF
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
# shellcheck source=systemd-lib.sh
source "$script_dir/systemd-lib.sh"
config_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"
state_home="${XDG_STATE_HOME:-$HOME/.local/state}"

unit="agentlibre-daemon.service"
cwd="$(git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null || printf '%s' "$repo_root")"
binary="${AGL_DAEMON_BINARY:-}"
if [[ -z "$binary" ]]; then
  binary="$(command -v agl || true)"
fi
function_ref="${AGL_DAEMON_FUNCTION:-}"
socket="${AGL_DAEMON_SOCKET:-$state_home/agentLIBRE/daemon/agl.sock}"
workspace_root="${AGL_DAEMON_WORKSPACE_ROOT:-$cwd}"
max_output_tokens="${AGL_DAEMON_MAX_OUTPUT_TOKENS:-256}"
tool_mode="${AGL_DAEMON_TOOL_MODE:-read-only}"
log_filter="${AGL_LOG:-agentlibre=info,warn}"
vulkan_driver_files=""
vulkan_driver_environment=""
vulkan_environment_line="UnsetEnvironment=VK_DRIVER_FILES VK_ICD_FILENAMES"$'\n'
if [[ -v VK_DRIVER_FILES ]]; then
  vulkan_driver_files="$VK_DRIVER_FILES"
  vulkan_driver_environment="VK_DRIVER_FILES"
elif [[ -v VK_ICD_FILENAMES ]]; then
  vulkan_driver_files="$VK_ICD_FILENAMES"
  vulkan_driver_environment="VK_ICD_FILENAMES"
fi
enable=0
restart=0
dry_run=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --unit)
      unit="${2:?missing value for --unit}"
      shift 2
      ;;
    --cwd)
      cwd="${2:?missing value for --cwd}"
      shift 2
      ;;
    --binary)
      binary="${2:?missing value for --binary}"
      shift 2
      ;;
    --function)
      function_ref="${2:?missing value for --function}"
      shift 2
      ;;
    --socket)
      socket="${2:?missing value for --socket}"
      shift 2
      ;;
    --workspace-root)
      workspace_root="${2:?missing value for --workspace-root}"
      shift 2
      ;;
    --max-output-tokens)
      max_output_tokens="${2:?missing value for --max-output-tokens}"
      shift 2
      ;;
    --tool-mode)
      tool_mode="${2:?missing value for --tool-mode}"
      shift 2
      ;;
    --log-filter)
      log_filter="${2:?missing value for --log-filter}"
      shift 2
      ;;
    --enable)
      enable=1
      shift
      ;;
    --restart)
      restart=1
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
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$binary" ]]; then
  echo "agl is not installed on PATH; install the runtime bundle or pass --binary" >&2
  exit 1
fi
requested_binary="$(realpath -m -s -- "$binary")"
resolved_binary="$(readlink -f -- "$requested_binary" 2>/dev/null || true)"
if [[ -z "$resolved_binary" ]]; then
  echo "agl binary does not resolve to an installed runtime generation: $requested_binary" >&2
  exit 1
fi

generation_dir="$(dirname -- "$resolved_binary")"
generations_dir="$(dirname -- "$generation_dir")"
runtime_dir="$(dirname -- "$generations_dir")"
libexec_dir="$(dirname -- "$runtime_dir")"
runtime_root="$(dirname -- "$libexec_dir")"
generation_name="$(basename -- "$generation_dir")"
binary="$runtime_root/bin/agl"
surface_worker="$runtime_root/bin/agl-inference-worker"
forbidden_surface_launcher="$runtime_root/bin/agl-process-launcher"
current_link="$runtime_dir/current"
engine="$generation_dir/llama-server"
runtime_manifest="$generation_dir/runtime-manifest.json"

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
    )" || return 1
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
  collect_nix_runtime_references "$generation" references || return 1
  if (( ${#references[@]} == 0 )); then
    [[ ! -e "$root_directory" && ! -L "$root_directory" ]] || return 1
    return 0
  fi
  [[ -d "$root_directory" && ! -L "$root_directory" &&
    "$(stat -c '%a' -- "$root_directory" 2>/dev/null || true)" == 555 &&
    "$(stat -c '%u' -- "$root_directory" 2>/dev/null || true)" == "$(id -u)" ]] || return 1
  for target in "${references[@]}"; do
    expected["$(basename -- "$target")"]="$target"
  done
  remaining=${#references[@]}
  shopt -s nullglob
  for entry in "$root_directory"/* "$root_directory"/.[!.]* "$root_directory"/..?*; do
    name="${entry##*/}"
    target="${expected[$name]:-}"
    [[ -n "$target" && -L "$entry" && "$(readlink -- "$entry")" == "$target" &&
      "$(readlink -f -- "$entry" 2>/dev/null || true)" == "$target" ]] || return 1
    unset 'expected[$name]'
    remaining=$((remaining - 1))
  done
  shopt -u nullglob
  (( remaining == 0 ))
}

managed_layout_error="agl must resolve through an immutable runtime bundle installed by scripts/install-agl-cargo.sh"
if [[ "$(basename -- "$resolved_binary")" != "agl" ||
      ! "$generation_name" =~ ^generation-[A-Za-z0-9]+$ ||
      "$(basename -- "$generations_dir")" != "generations" ||
      "$(basename -- "$runtime_dir")" != "agentlibre" ||
      "$(basename -- "$libexec_dir")" != "libexec" ||
      ! -f "$resolved_binary" || -L "$resolved_binary" ||
      ! -f "$engine" || -L "$engine" ||
      ! -f "$runtime_manifest" || -L "$runtime_manifest" ||
      -e "$surface_worker" || -L "$surface_worker" ||
      -e "$forbidden_surface_launcher" || -L "$forbidden_surface_launcher" ||
      ! -L "$current_link" ||
      "$(readlink -- "$current_link")" != "generations/$generation_name" ||
      "$(readlink -f -- "$current_link" 2>/dev/null || true)" != "$generation_dir" ||
      ! -L "$binary" ||
      "$(readlink -- "$binary")" != "../libexec/agentlibre/current/agl" ||
      "$(readlink -f -- "$binary" 2>/dev/null || true)" != "$resolved_binary" ||
      "$(realpath -e -- "$runtime_root/bin" 2>/dev/null || true)" != "$runtime_root/bin" ||
      "$(stat -c '%a' -- "$generation_dir" 2>/dev/null || true)" != "555" ||
      "$(stat -c '%a' -- "$resolved_binary" 2>/dev/null || true)" != "555" ||
      "$(stat -c '%a' -- "$engine" 2>/dev/null || true)" != "555" ||
      "$(stat -c '%u' -- "$resolved_binary" 2>/dev/null || true)" != "$(id -u)" ||
      "$(stat -c '%u' -- "$engine" 2>/dev/null || true)" != "$(id -u)" ||
      "$(stat -c '%h' -- "$resolved_binary" 2>/dev/null || true)" != "1" ||
      "$(stat -c '%h' -- "$engine" 2>/dev/null || true)" != "1" ||
      "$(stat -c '%a' -- "$runtime_manifest" 2>/dev/null || true)" != "444" ||
      "$(stat -c '%u' -- "$runtime_manifest" 2>/dev/null || true)" != "$(id -u)" ||
      "$(stat -c '%h' -- "$runtime_manifest" 2>/dev/null || true)" != "1" ||
      "$(stat -c '%u' -- "$generation_dir" 2>/dev/null || true)" != "$(id -u)" ]]; then
  echo "$managed_layout_error: $requested_binary" >&2
  exit 1
fi

current_uid="$(id -u)"
managed_ancestors=(
  "$runtime_root"
  "$runtime_root/bin"
  "$libexec_dir"
  "$runtime_dir"
  "$generations_dir"
)
for managed_ancestor in "${managed_ancestors[@]}"; do
  if [[ ! -d "$managed_ancestor" || -L "$managed_ancestor" ||
        "$(realpath -e -- "$managed_ancestor" 2>/dev/null || true)" != "$managed_ancestor" ]]; then
    echo "$managed_layout_error: non-canonical managed ancestor $managed_ancestor" >&2
    exit 1
  fi
  ancestor_owner="$(stat -c '%u' -- "$managed_ancestor" 2>/dev/null || true)"
  ancestor_mode="$(stat -c '%a' -- "$managed_ancestor" 2>/dev/null || true)"
  if [[ "$ancestor_owner" != "$current_uid" ]]; then
    echo "managed runtime ancestor must be owned by UID $current_uid: $managed_ancestor (owner $ancestor_owner)" >&2
    exit 1
  fi
  if [[ ! "$ancestor_mode" =~ ^[0-7]{3,4}$ ]] || (( (8#$ancestor_mode & 0022) != 0 )); then
    echo "managed runtime ancestor must not be group/other writable: $managed_ancestor (mode $ancestor_mode)" >&2
    exit 1
  fi
done

if ! command -v readelf >/dev/null 2>&1 ||
  ! validate_nix_runtime_roots "$generation_dir"; then
  echo "$managed_layout_error: incomplete or invalid Nix runtime GC roots in $generation_dir" >&2
  exit 1
fi

agl_systemd_validate_unit_name "$unit"
if [[ "$unit" != *.service ]]; then
  echo "--unit must end in .service: $unit" >&2
  exit 2
fi
agl_systemd_validate_absolute_vars \
  cwd \
  requested_binary \
  binary \
  resolved_binary \
  engine \
  runtime_manifest \
  socket \
  workspace_root

if [[ ! "$max_output_tokens" =~ ^[1-9][0-9]*$ ]]; then
  echo "--max-output-tokens must be a positive integer: $max_output_tokens" >&2
  exit 2
fi

case "$tool_mode" in
  read-only|write|execute|approve|admin) ;;
  *)
    echo "--tool-mode must be read-only, write, execute, approve, or admin: $tool_mode" >&2
    exit 2
    ;;
esac

agl_systemd_validate_nonempty_no_newline "--log-filter" "$log_filter"
function_argument=""
if [[ -n "$function_ref" ]]; then
  agl_systemd_validate_nonempty_no_newline "--function" "$function_ref"
  function_argument=" --function $(agl_systemd_quote "$function_ref")"
fi
if [[ -n "$vulkan_driver_environment" ]]; then
  if [[ -z "$vulkan_driver_files" ||
        "$vulkan_driver_files" == *[[:cntrl:]]* ||
        "$vulkan_driver_files" == :* ||
        "$vulkan_driver_files" == *: ||
        "$vulkan_driver_files" == *::* ]]; then
    echo "$vulkan_driver_environment must select nonempty colon-separated Vulkan manifest paths without control characters" >&2
    exit 2
  fi
  vulkan_driver_files_bytes="$(LC_ALL=C printf '%s' "$vulkan_driver_files" | wc -c)"
  if (( vulkan_driver_files_bytes > 32 * 1024 )); then
    echo "$vulkan_driver_environment must be at most 32768 bytes" >&2
    exit 2
  fi
  IFS=: read -r -a vulkan_driver_manifests <<<"$vulkan_driver_files"
  if (( ${#vulkan_driver_manifests[@]} > 16 )); then
    echo "$vulkan_driver_environment must select at most 16 Vulkan manifests" >&2
    exit 2
  fi
  for vulkan_driver_manifest in "${vulkan_driver_manifests[@]}"; do
    agl_systemd_validate_absolute_path "$vulkan_driver_environment entry" "$vulkan_driver_manifest"
    agl_systemd_require_file "$dry_run" "$vulkan_driver_manifest" "Vulkan driver manifest"
  done
  vulkan_environment_line="Environment=$(agl_systemd_quote "VK_DRIVER_FILES=$vulkan_driver_files")"$'\n'
  vulkan_environment_line+="UnsetEnvironment=VK_ICD_FILENAMES"$'\n'
fi
agl_systemd_require_dir "$dry_run" "$cwd" "working directory"
agl_systemd_require_dir "$dry_run" "$workspace_root" "workspace root"
agl_systemd_require_executable "$dry_run" "$binary"
agl_systemd_require_executable "$dry_run" "$engine"
if [[ "$dry_run" -eq 0 ]] &&
  ! env AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE=1 "$resolved_binary" >/dev/null; then
  echo "agl and its private llama-server do not have matching runtime identities: $resolved_binary" >&2
  exit 1
fi
agl_systemd_prepare_private_socket_parent "$dry_run" "$socket"

unit_dir="$config_home/systemd/user"
unit_file="$unit_dir/$unit"
socket_unit="${unit%.service}.socket"
socket_unit_file="$unit_dir/$socket_unit"
service_content="[Unit]
Description=agentLIBRE daemon
Requires=$socket_unit agl-terminald.service
After=$socket_unit agl-terminald.service

[Service]
Type=simple
UMask=0077
WorkingDirectory=$cwd
Environment=AGL_LOG=$log_filter
Environment=AGL_LOG_STDERR=always
${vulkan_environment_line}ExecStart=$(agl_systemd_quote "$binary") serve --systemd-activation --workspace-root $(agl_systemd_quote "$workspace_root")${function_argument} --max-output-tokens $max_output_tokens --tool-mode $tool_mode
Restart=on-failure
RestartSec=5
"

socket_content="[Unit]
Description=agentLIBRE daemon socket

[Socket]
ListenStream=$socket
FileDescriptorName=agentlibre
SocketMode=0600
DirectoryMode=0700
RemoveOnStop=true
Accept=no
Service=$unit

[Install]
WantedBy=sockets.target
"

echo "service unit: $unit"
echo "socket unit: $socket_unit"
echo "cwd: $cwd"
echo "requested binary: $requested_binary"
echo "binary: $binary"
echo "resolved binary: $resolved_binary"
echo "private llama-server: $engine"
echo "runtime manifest: $runtime_manifest"
echo "Nix runtime roots: $generation_dir/.nix-gc-roots (required only for Nix-linked ELF objects)"
if [[ -n "$function_ref" ]]; then
  echo "function profile: $function_ref"
else
  echo "function profile: workspace default"
fi
echo "socket: $socket"
echo "workspace root: $workspace_root"
echo "max output tokens: $max_output_tokens"
echo "tool mode: $tool_mode"
echo "log filter: $log_filter"
if [[ -n "$vulkan_driver_environment" ]]; then
  echo "Vulkan driver manifests: $vulkan_driver_files (from $vulkan_driver_environment)"
else
  echo "Vulkan driver manifests: none (CPU-only discovery)"
fi
echo "unit file: $unit_file"
echo "socket unit file: $socket_unit_file"

agl_systemd_print_or_install_user_unit \
  "$dry_run" \
  "$unit_dir" \
  "$unit" \
  "$service_content" \
  0 \
  0

agl_systemd_print_or_install_user_unit \
  "$dry_run" \
  "$unit_dir" \
  "$socket_unit" \
  "$socket_content" \
  "$enable" \
  "$restart"
