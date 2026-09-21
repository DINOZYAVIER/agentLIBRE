#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/uninstall-agl-cargo.sh [--root PATH] [--apply]

Prints the exact direct-install files that would be removed. Pass --apply to
remove them. Configuration, state and systemd units are preserved.
EOF
}

root=""
apply=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root) root="${2:?missing value for --root}"; shift 2 ;;
    --apply) apply=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$root" ]]; then
  root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-${HOME:?HOME is required}/.cargo}}"
fi
[[ "$root" == /* ]] || root="$PWD/$root"
root="$(realpath -m -s -- "$root")"
[[ "$(realpath -m -- "$root")" == "$root" ]] || {
  echo "installation prefix traverses a symlink: $root" >&2
  exit 1
}

binary="$root/bin/agl"
execd="$root/bin/agl-execd"
engine="$root/libexec/agentlibre"
if [[ ! -e "$binary" && ! -L "$binary" && ! -e "$execd" && ! -L "$execd" && ! -e "$engine" && ! -L "$engine" ]]; then
  echo "no direct agentLIBRE installation is present under $root"
  exit 0
fi

[[ ! -L "$binary" && ( ! -e "$binary" || -f "$binary" ) ]] || {
  echo "refusing unexpected agl path: $binary" >&2
  exit 1
}
[[ ! -L "$execd" && ( ! -e "$execd" || -f "$execd" ) ]] || {
  echo "refusing unexpected agl-execd path: $execd" >&2
  exit 1
}
[[ ! -L "$engine" && ( ! -e "$engine" || -d "$engine" ) ]] || {
  echo "refusing unexpected engine path: $engine" >&2
  exit 1
}

echo "remove: $binary"
echo "remove: $execd"
echo "remove: $engine"
if [[ "$apply" -eq 0 ]]; then
  echo "preview only; pass --apply to remove"
  exit 0
fi

rm -f -- "$binary" "$execd"
if [[ -d "$engine" ]]; then
  find "$engine" -mindepth 1 -maxdepth 1 -type f -delete
  rmdir -- "$engine"
fi
rmdir --ignore-fail-on-non-empty "$root/libexec" 2>/dev/null || true
echo "direct agentLIBRE installation removed"
