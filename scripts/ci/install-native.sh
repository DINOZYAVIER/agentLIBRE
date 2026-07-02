#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "$script_dir/lib.sh"

usage() {
  cat <<'EOF'
Usage: scripts/ci/install-native.sh --target <target> [--dry-run]

Targets:
  ubuntu-apt    Install native dependencies with apt-get.
  nixos         Validate Nix tooling and required native tools; no host mutation.
  arch-pacman   Install native dependencies with pacman.
  fedora-dnf    Install native dependencies with dnf.
  auto          Best-effort local target detection; refuses ambiguous systems.

Options:
  --help        Show this help.
  --target      Target to run.
  --dry-run     Print deterministic target actions without installing packages.
EOF
}

target=""
dry_run=0
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --target)
      [[ "$#" -ge 2 ]] || ci_fail "--target requires a value"
      target="$2"
      shift 2
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    *)
      ci_fail "unknown install-native option: $1"
      ;;
  esac
done

if [[ -z "$target" ]]; then
  ci_fail "missing --target; run scripts/ci/install-native.sh --help"
fi

detect_auto_target() {
  local matches=()
  command -v apt-get >/dev/null 2>&1 && matches+=(ubuntu-apt)
  command -v pacman >/dev/null 2>&1 && matches+=(arch-pacman)
  command -v dnf >/dev/null 2>&1 && matches+=(fedora-dnf)
  if command -v nix >/dev/null 2>&1 || command -v nix-shell >/dev/null 2>&1; then
    matches+=(nixos)
  fi

  if [[ "${#matches[@]}" -eq 1 ]]; then
    printf '%s\n' "${matches[0]}"
    return 0
  fi
  if [[ "${#matches[@]}" -eq 0 ]]; then
    ci_fail "auto target could not find apt-get, pacman, dnf, or Nix tooling; pass --target explicitly"
  fi
  ci_fail "auto target is ambiguous: ${matches[*]}; pass --target explicitly"
}

if [[ "$target" == "auto" ]]; then
  target="$(detect_auto_target)"
fi

case "$target" in
  ubuntu-apt|nixos|arch-pacman|fedora-dnf)
    args=()
    [[ "$dry_run" -eq 1 ]] && args+=(--dry-run)
    exec "$script_dir/install-native/$target.sh" "${args[@]}"
    ;;
  *)
    ci_fail "unknown install-native target: $target"
    ;;
esac
