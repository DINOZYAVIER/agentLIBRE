#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/agentlibre-matrix-bridge-systemd-service.sh [OPTIONS]

Installs a user-systemd service for agl-matrix-bridge.

Options:
  --unit NAME          systemd user unit name
  --cwd PATH           working directory for the service
  --binary PATH        agl-matrix-bridge binary path
  --config PATH        bridge config TOML path
  --log-filter FILTER  tracing filter for AGL_MATRIX_LOG
  --enable             enable the unit
  --restart            restart the unit after writing it
  --dry-run            print the unit without writing it
  -h, --help           show this help

Defaults:
  --unit        agl-matrix-bridge.service
  --cwd         current git repo root, or current directory outside git
  --binary      ./target/release/agl-matrix-bridge under the repo root
  --config      ~/.config/agentLIBRE/matrix-bridge/config.toml
  --log-filter  agl_matrix_bridge=info,matrix_sdk=warn,warn
EOF
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
config_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"

unit="agl-matrix-bridge.service"
cwd="$(git -C "$repo_root" rev-parse --show-toplevel 2>/dev/null || printf '%s' "$repo_root")"
binary="${AGL_MATRIX_BRIDGE_BINARY:-$repo_root/target/release/agl-matrix-bridge}"
config="${AGL_MATRIX_BRIDGE_CONFIG:-$config_home/agentLIBRE/matrix-bridge/config.toml}"
log_filter="${AGL_MATRIX_LOG:-agl_matrix_bridge=info,matrix_sdk=warn,warn}"
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

if [[ "$unit" == */* || "$unit" == *$'\n'* || -z "$unit" ]]; then
  echo "--unit must be a unit name, not a path: $unit" >&2
  exit 2
fi

for value_name in cwd binary config; do
  value="${!value_name}"
  if [[ "$value" != /* ]]; then
    echo "--${value_name//_/-} must be absolute: $value" >&2
    exit 2
  fi
  if [[ "$value" == *$'\n'* ]]; then
    echo "--${value_name//_/-} must not contain newlines" >&2
    exit 2
  fi
done

if [[ "$log_filter" == *$'\n'* || -z "$log_filter" ]]; then
  echo "--log-filter must be non-empty and must not contain newlines" >&2
  exit 2
fi

if [[ "$dry_run" -eq 0 && ! -d "$cwd" ]]; then
  echo "working directory does not exist: $cwd" >&2
  exit 1
fi

if [[ "$dry_run" -eq 0 && ! -x "$binary" ]]; then
  echo "binary does not exist or is not executable: $binary" >&2
  exit 1
fi

if [[ "$dry_run" -eq 0 && ! -f "$config" ]]; then
  echo "config file does not exist: $config" >&2
  exit 1
fi

systemd_quote() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

unit_dir="$config_home/systemd/user"
unit_file="$unit_dir/$unit"
unit_content="[Unit]
Description=agentLIBRE Matrix bridge

[Service]
Type=simple
WorkingDirectory=$cwd
Environment=AGL_MATRIX_LOG=$log_filter
ExecStart=$(systemd_quote "$binary") sync --config $(systemd_quote "$config")
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"

echo "unit: $unit"
echo "cwd: $cwd"
echo "binary: $binary"
echo "config: $config"
echo "log filter: $log_filter"
echo "unit file: $unit_file"

if [[ "$dry_run" -eq 1 ]]; then
  printf '\n%s' "$unit_content"
  exit 0
fi

mkdir -p "$unit_dir"
tmp_file="$(mktemp "$unit_dir/.${unit}.XXXXXX")"
printf '%s' "$unit_content" > "$tmp_file"
chmod 0644 "$tmp_file"
mv "$tmp_file" "$unit_file"

systemctl --user daemon-reload
systemctl --user reset-failed "$unit" || true

if [[ "$enable" -eq 1 ]]; then
  systemctl --user enable "$unit"
fi

if [[ "$restart" -eq 1 ]]; then
  systemctl --user restart "$unit"
fi

systemctl --user show "$unit" -p UnitFileState -p ActiveState -p ExecStart
