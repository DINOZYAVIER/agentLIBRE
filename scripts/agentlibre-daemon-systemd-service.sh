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
  --config PATH         local inference config TOML path
  --socket PATH         daemon Unix socket path
  --workspace-root PATH workspace root passed to agl serve
  --max-output-tokens N max generated tokens per turn
  --tool-mode MODE      read-only or write
  --log-filter FILTER   tracing filter for AGL_LOG
  --enable              enable the socket unit
  --restart             restart the socket unit after writing it
  --dry-run             print both units without writing them
  -h, --help            show this help

Defaults:
  --unit              agentlibre-daemon.service
  --cwd               current git repo root, or current directory outside git
  --binary            installed agl resolved from PATH
  --config            ~/.config/agentLIBRE/inference/local.toml
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
config="${AGL_DAEMON_CONFIG:-$config_home/agentLIBRE/inference/local.toml}"
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
    --config)
      config="${2:?missing value for --config}"
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
surface_launcher="$runtime_root/bin/agl-process-launcher"
surface_worker="$runtime_root/bin/agl-inference-worker"
current_link="$runtime_dir/current"
launcher="$generation_dir/agl-process-launcher"
worker="$generation_dir/agl-inference-worker"
native_bundle="$generation_dir/agl-inference-native"

resolve_native_bundle_relative() {
  local worker_elf="$1"
  local dynamic
  local runpath
  local component
  local relative
  local -a matches=()
  command -v readelf >/dev/null 2>&1 || return 1
  [[ -f "$worker_elf" && ! -L "$worker_elf" ]] || return 1
  dynamic="$(LC_ALL=C readelf -d -- "$worker_elf")" || return 1
  while IFS= read -r runpath; do
    IFS=: read -r -a components <<<"$runpath"
    for component in "${components[@]}"; do
      if [[ "$component" == '$ORIGIN/'* ]]; then
        relative="${component#\$ORIGIN/}"
        if [[ "$relative" == agl-inference-native/* ]]; then
          [[ "$relative" =~ ^agl-inference-native/sha256-[0-9a-f]{64}$ ]] ||
            return 1
          matches+=("$relative")
        fi
      fi
    done
  done < <(sed -n -E 's/.*\((RPATH|RUNPATH)\).*\[(.*)\]/\2/p' <<<"$dynamic")
  (( ${#matches[@]} == 1 )) || return 1
  printf '%s\n' "${matches[0]}"
}

validate_native_bundle() {
  local base="$1"
  local worker_elf="$2"
  local relative
  local leaf_name
  local directory
  local base_entry
  local base_count=0
  [[ -d "$base" && ! -L "$base" ]] || return 1
  [[ "$(stat -c '%a' -- "$base" 2>/dev/null || true)" == 555 &&
    "$(stat -c '%u' -- "$base" 2>/dev/null || true)" == "$(id -u)" ]] || return 1
  relative="$(resolve_native_bundle_relative "$worker_elf")" || return 1
  leaf_name="${relative#agl-inference-native/}"
  shopt -s nullglob
  for base_entry in "$base"/* "$base"/.[!.]* "$base"/..?*; do
    ((base_count += 1))
    [[ "${base_entry##*/}" == "$leaf_name" ]] || return 1
  done
  shopt -u nullglob
  (( base_count == 1 )) || return 1
  directory="$base/$leaf_name"
  local entry
  local name
  local count=0
  local cpu_count=0
  local total_bytes=0
  local size
  local required
  [[ -d "$directory" && ! -L "$directory" ]] || return 1
  [[ "$(stat -c '%a' -- "$directory" 2>/dev/null || true)" == 555 &&
    "$(stat -c '%u' -- "$directory" 2>/dev/null || true)" == "$(id -u)" ]] || return 1
  shopt -s nullglob
  for entry in "$directory"/* "$directory"/.[!.]* "$directory"/..?*; do
    name="${entry##*/}"
    [[ -f "$entry" && ! -L "$entry" &&
      "$(stat -c '%a' -- "$entry" 2>/dev/null || true)" == 555 &&
      "$(stat -c '%u' -- "$entry" 2>/dev/null || true)" == "$(id -u)" &&
      "$(stat -c '%h' -- "$entry" 2>/dev/null || true)" == 1 ]] || return 1
    case "$name" in
      libllama-common.so.0 | libmtmd.so.0 | libllama.so.0 | libggml.so.0 | libggml-base.so.0 | libggml-vulkan.so) ;;
      libggml-cpu-*.so) cpu_count=$((cpu_count + 1)) ;;
      *) return 1 ;;
    esac
    size="$(stat -c '%s' -- "$entry" 2>/dev/null || true)"
    [[ "$size" =~ ^[0-9]+$ ]] || return 1
    (( size <= 1024 * 1024 * 1024 )) || return 1
    total_bytes=$((total_bytes + size))
    (( total_bytes <= 4 * 1024 * 1024 * 1024 )) || return 1
    count=$((count + 1))
    (( count <= 64 )) || return 1
  done
  shopt -u nullglob
  (( cpu_count > 0 )) || return 1
  for required in libllama-common.so.0 libmtmd.so.0 libllama.so.0 libggml.so.0 libggml-base.so.0; do
    [[ -f "$directory/$required" && ! -L "$directory/$required" ]] || return 1
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
      ! -f "$launcher" || -L "$launcher" ||
      ! -f "$worker" || -L "$worker" ||
      ! -d "$native_bundle" || -L "$native_bundle" ||
      -e "$surface_worker" || -L "$surface_worker" ||
      ! -L "$current_link" ||
      "$(readlink -- "$current_link")" != "generations/$generation_name" ||
      "$(readlink -f -- "$current_link" 2>/dev/null || true)" != "$generation_dir" ||
      ! -L "$binary" ||
      "$(readlink -- "$binary")" != "../libexec/agentlibre/current/agl" ||
      "$(readlink -f -- "$binary" 2>/dev/null || true)" != "$resolved_binary" ||
      ! -L "$surface_launcher" ||
      "$(readlink -- "$surface_launcher")" != "../libexec/agentlibre/current/agl-process-launcher" ||
      "$(readlink -f -- "$surface_launcher" 2>/dev/null || true)" != "$launcher" ||
      "$(realpath -e -- "$runtime_root/bin" 2>/dev/null || true)" != "$runtime_root/bin" ||
      "$(stat -c '%a' -- "$generation_dir" 2>/dev/null || true)" != "555" ||
      "$(stat -c '%a' -- "$resolved_binary" 2>/dev/null || true)" != "555" ||
      "$(stat -c '%a' -- "$launcher" 2>/dev/null || true)" != "555" ||
      "$(stat -c '%a' -- "$worker" 2>/dev/null || true)" != "555" ||
      "$(stat -c '%u' -- "$resolved_binary" 2>/dev/null || true)" != "$(id -u)" ||
      "$(stat -c '%u' -- "$launcher" 2>/dev/null || true)" != "$(id -u)" ||
      "$(stat -c '%u' -- "$worker" 2>/dev/null || true)" != "$(id -u)" ||
      "$(stat -c '%h' -- "$resolved_binary" 2>/dev/null || true)" != "1" ||
      "$(stat -c '%h' -- "$launcher" 2>/dev/null || true)" != "1" ||
      "$(stat -c '%h' -- "$worker" 2>/dev/null || true)" != "1" ||
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

if ! validate_native_bundle "$native_bundle" "$worker"; then
  echo "$managed_layout_error: invalid exact native inference bundle $native_bundle" >&2
  exit 1
fi
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
  launcher \
  worker \
  native_bundle \
  config \
  socket \
  workspace_root

if [[ ! "$max_output_tokens" =~ ^[1-9][0-9]*$ ]]; then
  echo "--max-output-tokens must be a positive integer: $max_output_tokens" >&2
  exit 2
fi

case "$tool_mode" in
  read-only|write) ;;
  *)
    echo "--tool-mode must be read-only or write: $tool_mode" >&2
    exit 2
    ;;
esac

agl_systemd_validate_nonempty_no_newline "--log-filter" "$log_filter"
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
agl_systemd_require_executable "$dry_run" "$launcher"
agl_systemd_require_executable "$dry_run" "$worker"
if [[ "$dry_run" -eq 0 ]] &&
  ! env AGL_INTERNAL_VERIFY_RUNTIME_BUNDLE=1 "$resolved_binary" >/dev/null; then
  echo "agl, its sibling process launcher, and its private inference worker do not have matching build identities: $resolved_binary" >&2
  exit 1
fi
agl_systemd_require_file "$dry_run" "$config" "config file"
agl_systemd_prepare_private_socket_parent "$dry_run" "$socket"

unit_dir="$config_home/systemd/user"
unit_file="$unit_dir/$unit"
socket_unit="${unit%.service}.socket"
socket_unit_file="$unit_dir/$socket_unit"
service_content="[Unit]
Description=agentLIBRE daemon
Requires=$socket_unit
After=$socket_unit

[Service]
Type=simple
UMask=0077
WorkingDirectory=$cwd
Environment=AGL_LOG=$log_filter
Environment=AGL_LOG_STDERR=always
${vulkan_environment_line}ExecStart=$(agl_systemd_quote "$binary") serve --systemd-activation --config $(agl_systemd_quote "$config") --workspace-root $(agl_systemd_quote "$workspace_root") --max-output-tokens $max_output_tokens --tool-mode $tool_mode
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
echo "process launcher: $launcher"
echo "private inference worker: $worker"
echo "native inference bundle: $native_bundle"
echo "Nix runtime roots: $generation_dir/.nix-gc-roots (required only for Nix-linked ELF objects)"
echo "config: $config"
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
