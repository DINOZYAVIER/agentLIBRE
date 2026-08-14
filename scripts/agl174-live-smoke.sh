#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
agl_bin="${AGL174_LIVE_AGL_BIN:-$repo_root/target/release/agl}"

[[ -x "$agl_bin" ]] || {
  printf 'agl174-live: missing release binary: %s\n' "$agl_bin" >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  printf 'agl174-live: jq is required\n' >&2
  exit 1
}
if command -v sqlite3 >/dev/null 2>&1; then
  sqlite=(sqlite3)
elif command -v nix >/dev/null 2>&1; then
  sqlite=(nix shell nixpkgs#sqlite --command sqlite3)
else
  printf 'agl174-live: sqlite3 or nix is required\n' >&2
  exit 1
fi

live_root="$(mktemp -d "${TMPDIR:-/tmp}/agl174-live.XXXXXXXX")"
cleanup() {
  rm -rf -- "$live_root"
}
trap cleanup EXIT

job_json="$live_root/job.json"
run_json="$live_root/run.json"
history_json="$live_root/history.json"
status_json="$live_root/status.json"

"$agl_bin" cron add \
  --home "$live_root" \
  --name "AGL-174 live store status" \
  --schedule '* * * * *' \
  --builtin store-status \
  --json >"$job_json"
job_id="$(jq -er '.id' "$job_json")"

"$agl_bin" cron run --home "$live_root" "$job_id" --now --json >"$run_json"
"$agl_bin" cron history --home "$live_root" "$job_id" --json >"$history_json"
"$agl_bin" store status --home "$live_root" --json >"$status_json"

schema_version="$(jq -er '.schema_version' "$status_json")"
jq -e \
  --arg schema "$schema_version" \
  '.run.status == "succeeded"
   and .run.supervisor_run_id == null
   and .run.result_ref == ("builtin:store-status:schema:" + $schema)
   and .idempotency.final_status == "completed"' \
  "$run_json" >/dev/null
jq -e \
  'length == 1
   and .[0].status == "succeeded"
   and .[0].supervisor_run_id == null' \
  "$history_json" >/dev/null

row_counts="$("${sqlite[@]}" -readonly \
  "$live_root/data/store/agentlibre.sqlite3" \
  "SELECT (SELECT COUNT(*) FROM runs),
          (SELECT COUNT(*) FROM run_steps),
          (SELECT COUNT(*) FROM cron_runs);")"
[[ "$row_counts" == '0|0|1' ]] || {
  printf 'agl174-live: unexpected durable row counts: %s\n' "$row_counts" >&2
  exit 1
}

printf 'agl174-live: passed builtin Cron without durable agent Run/Step (schema=%s)\n' \
  "$schema_version"
