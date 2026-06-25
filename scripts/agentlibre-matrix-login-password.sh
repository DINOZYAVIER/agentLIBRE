#!/usr/bin/env bash
set -euo pipefail
umask 077

usage() {
  cat <<'EOF'
Usage:
  scripts/agentlibre-matrix-login-password.sh [OPTIONS]

Log in the agentLIBRE Matrix bridge with an interactive password prompt, save
the session, optionally verify the configured trusted device, and restart the
user-systemd bridge service.

Options:
  --config PATH               bridge config TOML path
  --unit NAME                 systemd user unit name
  --username USERNAME         Matrix username; defaults to matrix.user_id
  --device-display-name NAME  Matrix device display name for this login
  --replace-session           allow replacing the session file; store must be empty
  --skip-login                reuse the configured session and only verify/restart
  --skip-build                do not build target/release/agl-matrix-bridge
  --skip-verify               do not run verify-device after login
  --skip-restart              do not restart the systemd bridge service
  -h, --help                  show this help

The Matrix password is read with terminal echo disabled and passed to
agl-matrix-bridge over stdin. It is not written to TOML, shell history,
environment variables, or command-line arguments.
EOF
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
config_home="${XDG_CONFIG_HOME:-${HOME:?HOME is required}/.config}"

config="$config_home/agentLIBRE/matrix-bridge/agl.toml"
unit="agl-matrix-bridge.service"
username=""
device_display_name="agl-matrix-bridge"
replace_session=0
login=1
build=1
verify=1
restart=1
bridge_stopped=0

restart_unit_on_exit() {
  if [[ "$bridge_stopped" -eq 1 && "$restart" -eq 1 ]]; then
    systemctl --user restart "$unit" 2>/dev/null || true
  fi
}

trap restart_unit_on_exit EXIT

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)
      config="${2:?missing value for --config}"
      shift 2
      ;;
    --unit)
      unit="${2:?missing value for --unit}"
      shift 2
      ;;
    --username)
      username="${2:?missing value for --username}"
      shift 2
      ;;
    --device-display-name)
      device_display_name="${2:?missing value for --device-display-name}"
      shift 2
      ;;
    --replace-session)
      replace_session=1
      shift
      ;;
    --skip-login)
      login=0
      shift
      ;;
    --skip-build)
      build=0
      shift
      ;;
    --skip-verify)
      verify=0
      shift
      ;;
    --skip-restart)
      restart=0
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

if [[ "$config" != /* ]]; then
  echo "--config must be absolute: $config" >&2
  exit 2
fi

if [[ "$unit" == */* || "$unit" == *$'\n'* || -z "$unit" ]]; then
  echo "--unit must be a unit name, not a path: $unit" >&2
  exit 2
fi

if [[ ! -f "$config" ]]; then
  echo "config file does not exist: $config" >&2
  exit 1
fi

bridge_bin="$repo_root/target/release/agl-matrix-bridge"

config_value() {
  local section="$1"
  local key="$2"
  awk -v section="$section" -v key="$key" '
    $0 ~ "^\\[" section "\\]" { in_section = 1; next }
    $0 ~ "^\\[" { in_section = 0 }
    in_section && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
      line = $0
      sub(/^[^"]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit
    }
  ' "$config"
}

if [[ -z "$username" ]]; then
  username="$(config_value matrix user_id)"
fi

if [[ -z "$username" ]]; then
  echo "Matrix username is required; set matrix.user_id or pass --username" >&2
  exit 2
fi

if [[ "$build" -eq 1 ]]; then
  cargo build --release -p agl-matrix-bridge
fi

"$script_dir/agentlibre-matrix-bridge-systemd-service.sh" \
  --unit "$unit" \
  --config "$config" \
  --enable

systemctl --user stop "$unit" 2>/dev/null || true
bridge_stopped=1

run_login() {
  local args=(
    login-password
    --config "$config" \
    --username "$username" \
    --password-stdin \
    --device-display-name "$device_display_name"
  )
  if [[ "$replace_session" -eq 1 ]]; then
    args+=(--replace-session)
  fi
  printf '%s\n' "$password" | "$bridge_bin" "${args[@]}"
}

if [[ "$login" -eq 1 ]]; then
  printf 'Matrix username: %s\n' "$username"
  printf 'Matrix password: '
  IFS= read -rs password
  printf '\n'

  if [[ -z "$password" ]]; then
    echo "Matrix password is empty" >&2
    exit 2
  fi

  if ! login_output="$(run_login 2>&1)"; then
    printf '%s\n' "$login_output" >&2
    unset password
    exit 1
  fi
  printf '%s\n' "$login_output"
  unset password
else
  echo "Skipping Matrix password login; reusing configured session." >&2
fi

"$bridge_bin" check-config --config "$config"

verification_device_id="$(config_value verification device_id)"

if [[ "$verify" -eq 1 && -n "$verification_device_id" ]]; then
  echo "Waiting for the trusted Matrix device to start verification for the new bridge device." >&2
  "$bridge_bin" verify-device --config "$config" --accept-incoming
elif [[ "$verify" -eq 1 ]]; then
  echo "Skipping device verification: set [verification].device_id in $config" >&2
fi

if [[ "$restart" -eq 1 ]]; then
  systemctl --user restart "$unit"
  bridge_stopped=0
  systemctl --user status "$unit" --no-pager
fi
