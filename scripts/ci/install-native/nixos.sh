#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
source "$script_dir/../lib.sh"

dry_run=0
if [[ "${1:-}" == "--dry-run" ]]; then
  dry_run=1
  shift
fi
if [[ "$#" -gt 0 ]]; then
  ci_fail "nixos target accepts only --dry-run"
fi

tools=(cmake git pkg-config)
native_tools=(glslc glslangValidator spirv-as)

if [[ "$dry_run" -eq 1 ]]; then
  ci_section "nixos native dependency dry run"
  printf 'target=nixos\n'
  printf 'mode=validate-only\n'
  printf 'required_tools=%s %s\n' "${tools[*]}" "${native_tools[*]}"
  printf 'hint=Use nix-shell nix/agl-local-vulkan.nix or a project Nix shell before running CI steps.\n'
  exit 0
fi

if ! command -v nix >/dev/null 2>&1 && ! command -v nix-shell >/dev/null 2>&1; then
  ci_fail "nixos target requires Nix tooling; use scripts/agl-nix-vulkan.sh or enter a project Nix shell"
fi

missing=()
for tool in "${tools[@]}" "${native_tools[@]}"; do
  command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [[ "${#missing[@]}" -gt 0 ]]; then
  ci_fail "nixos target is validate-only and found missing tools: ${missing[*]}; enter nix-shell nix/agl-local-vulkan.nix or a project Nix shell"
fi

ci_section "NixOS native dependency validation passed"
