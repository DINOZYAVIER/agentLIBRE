#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage:
  scripts/agl-nix-profile.sh build
  scripts/agl-nix-profile.sh run [--] [AGL_ARGS...]
  scripts/agl-nix-profile.sh install
  scripts/agl-nix-profile.sh history
  scripts/agl-nix-profile.sh rollback [GENERATION]

Build and check the committed AGL Flake, install or upgrade it in the
current user's Nix profile, inspect profile generations, or roll back.
USAGE
}

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

profile_action="${1:-help}"
if (($# > 0)); then
  shift
fi

case "$profile_action" in
  -h|--help|help)
    usage
    exit 0
    ;;
  history)
    exec nix profile history "$@"
    ;;
  rollback)
    if (($# > 1)); then
      usage >&2
      exit 2
    fi
    if (($# == 1)); then
      [[ "$1" =~ ^[0-9]+$ ]] || fail 'GENERATION must be a numeric profile version'
      exec nix profile rollback --to "$1"
    fi
    exec nix profile rollback
    ;;
  build|run|install)
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

command -v nix >/dev/null 2>&1 || fail 'nix is required'
command -v git >/dev/null 2>&1 || fail 'git is required'
command -v python3 >/dev/null 2>&1 || fail 'python3 is required'

branch="$(git -C "$repo_root" symbolic-ref --quiet --short HEAD)" ||
  fail 'check out a branch before building or installing AGL'

if ! git -C "$repo_root" diff --quiet HEAD --; then
  fail 'tracked changes are not included; commit them before building or installing a profile generation'
fi

printf 'using committed branch %s at %s (untracked files are excluded)\n' \
  "$branch" "$(git -C "$repo_root" rev-parse --short HEAD)" >&2

flake_url="git+file://${repo_root}?ref=${branch}"
installable="${flake_url}#agl"
expected_system="$(nix eval --impure --raw --expr builtins.currentSystem)"
expected_attr="packages.${expected_system}.agl"

build_and_check() {
  local out_path
  out_path="$(nix build --no-write-lock-file "$installable" --no-link --print-out-paths)"
  [[ -x "$out_path/bin/agl" ]] || fail "Nix output is missing bin/agl: $out_path"
  [[ -x "$out_path/bin/agl-matrix-bridge" ]] || fail "Nix output is missing bin/agl-matrix-bridge: $out_path"
  [[ -x "$out_path/libexec/agentlibre/llama-server" ]] || fail "Nix output is missing the private llama-server: $out_path"

  "$out_path/bin/agl" --version >/dev/null
  "$out_path/bin/agl-matrix-bridge" --version >/dev/null
  "$out_path/libexec/agentlibre/llama-server" --help >/dev/null 2>&1
  printf 'built and smoke-checked %s\n' "$out_path"
}

case "$profile_action" in
  build)
    (($# == 0)) || { usage >&2; exit 2; }
    build_and_check
    ;;
  run)
    if [[ "${1:-}" == "--" ]]; then
      shift
    fi
    exec nix run --no-write-lock-file "$installable" -- "$@"
    ;;
  install)
    (($# == 0)) || { usage >&2; exit 2; }
    build_and_check

    mapfile -t matching_names < <(
      nix profile list --json |
        python3 -c '
import json
import sys

expected_attr, expected_url = sys.argv[1:]
elements = json.load(sys.stdin).get("elements", {})
for name, element in elements.items():
    if not element.get("active"):
        continue
    if element.get("attrPath") == expected_attr and element.get("originalUrl") == expected_url:
        print(name)
' "$expected_attr" "$flake_url"
    )

    if ((${#matching_names[@]} > 1)); then
      fail 'the user profile contains multiple AGL entries from this Flake; remove duplicates with `nix profile remove <name>` before retrying'
    elif ((${#matching_names[@]} == 1)); then
      nix profile upgrade "${matching_names[0]}"
    else
      mapfile -t other_agl_names < <(
        nix profile list --json |
          python3 -c '
import json
import sys

expected_attr = sys.argv[1]
elements = json.load(sys.stdin).get("elements", {})
for name, element in elements.items():
    if element.get("active") and element.get("attrPath") == expected_attr:
        print(name)
' "$expected_attr"
      )
      if ((${#other_agl_names[@]} > 0)); then
        fail "AGL is already installed from another Flake as '${other_agl_names[0]}'; remove that profile entry explicitly, then retry"
      fi
      nix profile add "$installable"
    fi

    printf 'AGL profile generation is active. Run `%s history` to inspect it.\n' "$script_dir/agl-nix-profile.sh"
    ;;
esac
