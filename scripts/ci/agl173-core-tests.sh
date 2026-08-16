#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/../.." && pwd)"
cd "${repo_root}"

model_tests=(
  agl173_model_api
  agl173_install_transaction
)

inference_tests=(
  agl173_architecture
  agl173_attempt_journal
  agl173_host_api
)

for test_name in "${model_tests[@]}"; do
  cargo test -p agl-model --test "${test_name}"
done

for test_name in "${inference_tests[@]}"; do
  cargo test -p agl-inference --test "${test_name}"
done
