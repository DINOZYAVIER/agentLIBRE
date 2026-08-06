#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/../.." && pwd)"
cd "${repo_root}"

kernel_tests=(
  kernel_core_architecture
  kernel_core_extension
  kernel_core_hooks
  kernel_core_identifiers
  kernel_core_outcome
  kernel_core_policy
  kernel_core_schema
  kernel_core_session
  kernel_core_tool_effect
  kernel_core_turn
)

for test_name in "${kernel_tests[@]}"; do
  cargo test -p agl-kernel --test "${test_name}"
done

cargo test -p agl-runtime --test kernel_core_runtime
