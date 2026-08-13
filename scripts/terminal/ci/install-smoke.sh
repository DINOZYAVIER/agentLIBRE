#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../.." && pwd)"
temporary_root="$(mktemp -d)"
prefix="$temporary_root/prefix"

cleanup() {
  chmod -R u+w "$temporary_root" 2>/dev/null || true
  rm -rf -- "$temporary_root"
}
trap cleanup EXIT

"$repo_root/scripts/terminal/install.sh" --prefix "$prefix" --debug

public_service="$prefix/bin/agl-terminald"
public_ui="$prefix/bin/agl-terminal"
[[ -L "$public_ui" ]] || {
  printf 'install-smoke: public UI link is missing\n' >&2
  exit 1
}
[[ -L "$public_service" ]] || {
  printf 'install-smoke: public service link is missing\n' >&2
  exit 1
}
[[ ! -e "$prefix/bin/agl-process-launcher" ]] || {
  printf 'install-smoke: private launcher leaked onto PATH\n' >&2
  exit 1
}
service="$(realpath -e -- "$public_service")"
ui="$(realpath -e -- "$public_ui")"
generation="$(dirname -- "$service")"
launcher="$generation/agl-process-launcher"
manifest="$generation/runtime-manifest.json"
[[ "$(dirname -- "$ui")" == "$generation" ]] || {
  printf 'install-smoke: UI and service resolve to different generations\n' >&2
  exit 1
}
[[ -x "$ui" && -x "$service" && -x "$launcher" && -f "$manifest" ]] || {
  printf 'install-smoke: immutable generation is incomplete\n' >&2
  exit 1
}
[[ "$(stat -c '%a' -- "$generation")" == 555 ]] || {
  printf 'install-smoke: generation is writable\n' >&2
  exit 1
}

service_digest="sha256:$(sha256sum -- "$service" | awk '{print $1}')"
ui_digest="sha256:$(sha256sum -- "$ui" | awk '{print $1}')"
grep -F "\"sha256\": \"$ui_digest\"" "$manifest" >/dev/null || {
  printf 'install-smoke: manifest UI digest does not match installed bytes\n' >&2
  exit 1
}
grep -F "\"sha256\": \"$service_digest\"" "$manifest" >/dev/null || {
  printf 'install-smoke: manifest service digest does not match installed bytes\n' >&2
  exit 1
}
AGL_PROCESS_LAUNCH_DOCTOR=1 "$launcher" >/dev/null
"$public_ui" --version >/dev/null
HOME="$temporary_root/home" \
XDG_CONFIG_HOME="$temporary_root/config" \
XDG_DATA_HOME="$temporary_root/data" \
XDG_STATE_HOME="$temporary_root/state" \
XDG_RUNTIME_DIR="$temporary_root/runtime" \
  "$repo_root/scripts/terminal/systemd-user-service.sh" --prefix "$prefix" >/dev/null

retained_data="$prefix/share/agl-terminal/retained"
mkdir -p -- "$(dirname -- "$retained_data")"
touch "$retained_data"
"$repo_root/scripts/terminal/uninstall.sh" --prefix "$prefix"
"$repo_root/scripts/terminal/uninstall.sh" --prefix "$prefix" --apply

[[ ! -e "$public_ui" && ! -L "$public_ui" && ! -e "$public_service" &&
  ! -L "$public_service" && ! -e "$generation" ]] || {
  printf 'install-smoke: managed binaries remain after uninstall\n' >&2
  exit 1
}
[[ -f "$retained_data" ]] || {
  printf 'install-smoke: uninstall removed terminal data\n' >&2
  exit 1
}

printf 'agl-terminal-install-smoke: passed\n'
