#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/terminal/uninstall.sh [--prefix PATH] [--apply] [--dry-run]

Validates the managed agl-terminal and agl-terminald links and generation.
Without --apply the command only reports what would be removed. Terminal data
and state roots are never removed.
EOF
}

fail() {
  printf 'agl-terminal-uninstall: %s\n' "$*" >&2
  exit 1
}

prefix=""
apply=0
dry_run=0
while (($#)); do
  case "$1" in
    --prefix)
      shift
      (($#)) || fail "--prefix requires a path"
      prefix="$1"
      ;;
    --apply) apply=1 ;;
    --dry-run) dry_run=1 ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) fail "unknown option: $1" ;;
  esac
  shift
done

prefix="${prefix:-${HOME:?HOME is required}/.local}"
[[ "$prefix" == /* ]] || prefix="$PWD/$prefix"
prefix="$(realpath -m -s -- "$prefix")"
ui_link="$prefix/bin/agl-terminal"
service_link="$prefix/bin/agl-terminald"
product_root="$prefix/libexec/agl-terminal"
current_link="$product_root/current"

if ((dry_run)); then
  printf '+ validate %q and %q against one immutable managed generation\n' "$ui_link" "$service_link"
  printf '+ preview removal; terminal data and state are retained\n'
  exit 0
fi

if [[ ! -e "$ui_link" && ! -L "$ui_link" && ! -e "$service_link" && ! -L "$service_link" ]]; then
  printf 'agl-terminal-uninstall: no managed terminal links under %s\n' "$prefix"
  exit 0
fi
[[ -L "$ui_link" && -L "$service_link" && -L "$current_link" ]] ||
  fail "managed install is incomplete; public links and current generation must be present"
ui_target="$(readlink -- "$ui_link")"
service_target="$(readlink -- "$service_link")"
[[ "$service_target" == "../libexec/agl-terminal/current/agl-terminald" ]] ||
  fail "service command has an unexpected target: $service_target"
[[ "$ui_target" == "../libexec/agl-terminal/current/agl-terminal" ]] ||
  fail "UI command does not resolve through the managed current generation"
current_target="$(readlink -- "$current_link")"
[[ "$current_target" =~ ^generations/(generation-[0-9a-f]{64})$ ]] ||
  fail "current generation has an unexpected target: $current_target"
generation="${BASH_REMATCH[1]}"
generation_path="$prefix/libexec/agl-terminal/generations/$generation"
[[ -d "$generation_path" && ! -L "$generation_path" ]] ||
  fail "managed generation is unavailable: $generation_path"
current_uid="$(id -u)"
[[ "$(stat -c '%u' -- "$generation_path")" == "$current_uid" &&
  "$(stat -c '%a' -- "$generation_path")" == 555 ]] ||
  fail "managed generation ownership or mode is invalid: $generation_path"
for name in agl-terminal agl-terminald agl-process-launcher runtime-manifest.json; do
  [[ -f "$generation_path/$name" && ! -L "$generation_path/$name" ]] ||
    fail "managed generation entry is invalid: $generation_path/$name"
  [[ "$(stat -c '%u' -- "$generation_path/$name")" == "$current_uid" &&
    "$(stat -c '%h' -- "$generation_path/$name")" == 1 ]] ||
    fail "managed generation entry ownership is invalid: $generation_path/$name"
done
[[ "$(stat -c '%a' -- "$generation_path/agl-terminal")" == 555 &&
  "$(stat -c '%a' -- "$generation_path/agl-terminald")" == 555 &&
  "$(stat -c '%a' -- "$generation_path/agl-process-launcher")" == 555 &&
  "$(stat -c '%a' -- "$generation_path/runtime-manifest.json")" == 444 ]] ||
  fail "managed generation entry modes are invalid: $generation_path"

printf 'ui_link=%s\n' "$ui_link"
printf 'service_link=%s\n' "$service_link"
printf 'generation=%s\n' "$generation_path"
printf 'terminal_data=retained\nterminal_state=retained\n'
if ((apply)); then
  if command -v systemctl >/dev/null 2>&1 && systemctl --user is-active --quiet agl-terminald.service; then
    fail "agl-terminald.service is still active; stop it before uninstall"
  fi
  operation_lock="$product_root/.operation.lock"
  [[ -f "$operation_lock" && ! -L "$operation_lock" &&
    "$(stat -c '%u:%a:%h' -- "$operation_lock")" == "$(id -u):600:1" ]] ||
    fail "terminal operation lock is not private and single-link: $operation_lock"
  exec 9<>"$operation_lock"
  flock -xn 9 || fail "another terminal install/uninstall operation holds $operation_lock"
  rm -- "$ui_link" "$service_link" "$current_link"
  chmod u+w "$generation_path"
  rm -rf -- "$generation_path"
  printf 'agl-terminal-uninstall: removed managed binaries\n'
else
  printf 'agl-terminal-uninstall: preview only; pass --apply after stopping agl-terminald\n'
fi
