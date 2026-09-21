#!/usr/bin/env bash
set -euo pipefail

# Real-model verification for reports/planning-implementation-flow.md item 7.
# Runtime state is temporary; only the concise report is written to reports/.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
artifact_root="${AGL_E2E_ARTIFACT_ROOT:-/tmp/agl-planning-implementation-e2e}"
run_root="$artifact_root/$(date +%Y%m%d-%H%M%S)-$$"
agl_home="$run_root/agl-home"
workspace="$run_root/workspace"
forge_bin="${AGL_E2E_FORGE_BIN:-$repo_root/../ayeque-forge/target/debug/ayeque-forge}"
agl_bin="${AGL_E2E_AGL_BIN:-$repo_root/target/debug/agl}"
execd_bin="${AGL_E2E_EXECD_BIN:-$repo_root/target/debug/agl-execd}"
engine="${AGL_E2E_LLAMA_SERVER_BIN:-$HOME/.cargo/libexec/agentlibre/llama-server}"
planner_function="${AGL_E2E_PLANNER_FUNCTION:-$HOME/repos/agentlibre-functions/functions/qwen38-27b-reasoning}"
coder_function="${AGL_E2E_CODER_FUNCTION:-$HOME/repos/agentlibre-functions/functions/search-qwen38-27b-reasoning}"
planner_source="$planner_function"
coder_source="$coder_function"
planner_model="${AGL_E2E_PLANNER_MODEL_GGUF:-$HOME/.local/share/agentLIBRE/runtime/models/3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e.gguf}"
coder_model="${AGL_E2E_CODER_MODEL_GGUF:-$HOME/.local/share/agentLIBRE/runtime/models/3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e.gguf}"
planner_model_digest="${AGL_E2E_PLANNER_MODEL_DIGEST:-3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e}"
coder_model_digest="${AGL_E2E_CODER_MODEL_DIGEST:-3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e}"
agents_repo="${AGL_E2E_AGENTS_REPO:-$HOME/repos/agentlibre-agents}"
models_repo="${AGL_E2E_MODELS_REPO:-$HOME/repos/agentlibre-models}"
extensions_repo="${AGL_E2E_EXTENSIONS_REPO:-$HOME/repos/agentlibre-extensions}"
functions_repo="${AGL_E2E_FUNCTIONS_REPO:-$HOME/repos/agentlibre-functions}"
timeout_seconds="${AGL_E2E_TIMEOUT_SECONDS:-1800}"
report_path="${AGL_E2E_REPORT_PATH:-$repo_root/reports/planning-implementation-e2e.md}"
search_certificate="${AGL_E2E_SEARCH_CERTIFICATE:-$HOME/.config/agentLIBRE/credentials/ayeque-search/client.crt}"
search_private_key="${AGL_E2E_SEARCH_PRIVATE_KEY:-$HOME/.config/agentLIBRE/credentials/ayeque-search/client.pk8}"
search_ca="${AGL_E2E_SEARCH_CA:-$HOME/.config/agentLIBRE/credentials/ayeque-search/ca.crt}"
daemon_log="$run_root/daemon.log"
execd_log="$run_root/execd.log"

die() {
  echo "planning E2E: $*" >&2
  echo "artifacts: $run_root" >&2
  exit 1
}
need() {
  [[ -n "$2" && "$2" = /* && -e "$2" ]] || die "$1 is unavailable: $2"
}
need_exec() {
  need "$1" "$2"
  [[ -x "$2" ]] || die "$1 is not executable: $2"
}
need "planner Function" "$planner_function"
need "coder Function" "$coder_function"
need "planner model" "$planner_model"
need "coder model" "$coder_model"
need "Agents repository" "$agents_repo"
need "Models repository" "$models_repo"
need "Extensions repository" "$extensions_repo"
need "Functions repository" "$functions_repo"
need "search client certificate" "$search_certificate"
need "search client private key" "$search_private_key"
need "search CA" "$search_ca"
if [[ ! -e "$forge_bin" ]]; then
  forge_bin="$(command -v ayeque-forge || true)"
fi
need_exec ayeque-forge "$forge_bin"
need_exec agl "$agl_bin"
need_exec agl-execd "$execd_bin"
need_exec llama-server "$engine"
command -v python3 >/dev/null || die "python3 is required"
command -v git >/dev/null || die "git is required"
command -v timeout >/dev/null || die "timeout is required"

# The interactive shell may inherit systemd socket-activation variables from
# the installed agentLIBRE service.  This harness owns its temporary socket;
# do not make the isolated execd try to claim fd 3 from that service.
unset LISTEN_FDS LISTEN_PID LISTEN_FDNAMES LISTEN_PIDFDID

mkdir -p "$run_root" "$agl_home/config" "$agl_home/data/runtime/models" "$agl_home/state"
mkdir -p "$(dirname -- "$report_path")"
execd_pid=""
daemon_pid=""
export AGL_E2E_DAEMON_SOCKET="$agl_home/state/daemon/agl.sock"
export AGL_E2E_EXECD_SOCKET="$agl_home/state/execd/execd.sock"
cleanup() {
  if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  if [[ -n "$execd_pid" ]] && kill -0 "$execd_pid" 2>/dev/null; then
    kill "$execd_pid" 2>/dev/null || true
    wait "$execd_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

export AGL_E2E_RUN_ROOT="$run_root"
export AGL_E2E_WORKSPACE="$workspace"
export AGL_E2E_AGL_HOME="$agl_home"
export AGL_E2E_FUNCTIONS_REPO="$functions_repo"
export AGL_E2E_AGENTS_REPO="$agents_repo"
export AGL_E2E_MODELS_REPO="$models_repo"
export AGL_E2E_EXTENSIONS_REPO="$extensions_repo"
export AGL_E2E_PLANNER_FUNCTION="$planner_function"
export AGL_E2E_CODER_FUNCTION="$coder_function"
export AGL_E2E_ENGINE="$engine"
export AGL_E2E_PLANNER_MODEL="$planner_model"
export AGL_E2E_CODER_MODEL="$coder_model"
export AGL_E2E_REPORT_PATH="$report_path"
export AGL_E2E_REPO_ROOT="$repo_root"
export AGL_E2E_PLANNER_SOURCE="$planner_source"
export AGL_E2E_CODER_SOURCE="$coder_source"
export AGL_E2E_PLANNER_MODEL_DIGEST="$planner_model_digest"
export AGL_E2E_CODER_MODEL_DIGEST="$coder_model_digest"
export AGL_E2E_SEARCH_CERTIFICATE="$search_certificate"
export AGL_E2E_SEARCH_PRIVATE_KEY="$search_private_key"
export AGL_E2E_SEARCH_CA="$search_ca"

python3 - <<'PY'
import os
from pathlib import Path
root = Path(os.environ["AGL_E2E_WORKSPACE"])
root.mkdir(parents=True, exist_ok=True)
(root / "hello.py").write_text(
    "def greeting(name):\n    return f\"Hello, {name}!\"\n", encoding="utf-8")
(root / "test_hello.py").write_text(
    "from hello import greeting\n\n\n"
    "def test_greeting():\n    assert greeting(\"Ada\") == \"Hello, Ada!\"\n",
    encoding="utf-8")
(root / "DECISIONS.md").write_text(
    "# Human decision record\n\n"
    "- D1: Add public shout(name) to hello.py; it returns greeting(name).upper() exactly.\n"
    "- D2: Add the regression test to test_hello.py.\n"
    "- D3: Use two sequential slices: implementation first, test second.\n"
    "- D4: Do not change any other file or public behavior.\n", encoding="utf-8")
(root / "README.md").write_text(
    "This fixture is intentionally small. DECISIONS.md is authoritative.\n",
    encoding="utf-8")
PY

git -C "$workspace" init --quiet
git -C "$workspace" config user.name agentLIBRE-e2e
git -C "$workspace" config user.email agentlibre-e2e.invalid
git -C "$workspace" add --all
git -C "$workspace" commit --quiet -m 'Create planning E2E fixture'
fixture_commit="$(git -C "$workspace" rev-parse HEAD)"

export XDG_CONFIG_HOME="$agl_home/config"
export XDG_DATA_HOME="$agl_home/data"
"$forge_bin" init "$workspace" >/dev/null
forge_project="$agl_home/data/ayeque-forge/projects/workspace/FORGE.toml"
[[ -f "$forge_project" ]] || die "Forge init did not create a project manifest"
export AGL_E2E_FORGE_MANIFEST="$forge_project"

python3 - <<'PY'
import os
import subprocess
import tomllib
from pathlib import Path
def rev(path):
    return subprocess.check_output(["git", "-C", path, "rev-parse", "HEAD"], text=True).strip()
def fn(path):
    with open(Path(path) / "FUNCTION.toml", "rb") as handle:
        return tomllib.load(handle)
planner_path = os.environ["AGL_E2E_PLANNER_SOURCE"]
coder_path = os.environ["AGL_E2E_CODER_SOURCE"]
functions = [(planner_path, fn(planner_path)), (coder_path, fn(coder_path))]
entities = {}
def add(identifier, kind, schema, git, path):
    entities[(kind, identifier)] = {
        "id": identifier, "kind": kind, "schema": schema, "git": git,
        "revision": rev(git), "path": path}
add("agentlibre.chat-agent", "agent", "agentlibre.agent/v1",
    os.environ["AGL_E2E_AGENTS_REPO"], "agents/chat")
model_paths = {"agentlibre.gemma4-12b": "models/gemma4-12b",
               "agentlibre.qwen38-27b": "models/qwen38-27b"}
for _, function in functions:
    add(function["model"]["id"], "model", "agentlibre.model/v1",
        os.environ["AGL_E2E_MODELS_REPO"], model_paths[function["model"]["id"]])
extension_paths = {
    "agentlibre.builtins": (os.environ["AGL_E2E_EXTENSIONS_REPO"], "extensions/builtins"),
    "agentlibre.searxng": (os.environ["AGL_E2E_EXTENSIONS_REPO"], "extensions/searxng"),
    "agentlibre.execution": (os.environ["AGL_E2E_REPO_ROOT"], "extensions/agentlibre-execution"),
}
for _, function in functions:
    for extension in function.get("extensions", []):
        git, path = extension_paths[extension["id"]]
        add(extension["id"], "extension", "agentlibre.extension/v1", git, path)
for function_path, function in functions:
    add(function["id"], "function", "agentlibre.function/v2",
        os.environ["AGL_E2E_FUNCTIONS_REPO"],
        str(Path(function_path).relative_to(os.environ["AGL_E2E_FUNCTIONS_REPO"])))
lines = ["format = 2", ""]
for entity in sorted(entities.values(), key=lambda e: (e["kind"], e["id"])):
    lines += ["[[entity]]", f'id = "{entity["id"]}"',
              f'kind = "{entity["kind"]}"', f'schema = "{entity["schema"]}"',
              f'git = "{entity["git"]}"', f'revision = "{entity["revision"]}"',
              f'path = "{entity["path"]}"', ""]
Path(os.environ["AGL_E2E_FORGE_MANIFEST"]).write_text("\n".join(lines), encoding="utf-8")
PY
(cd "$workspace" && "$forge_bin" lock >/dev/null)
(cd "$workspace" && "$forge_bin" validate >/dev/null)
planner_id="$(python3 -c 'import tomllib,sys; print(tomllib.load(open(sys.argv[1], "rb"))["id"])' "$planner_source/FUNCTION.toml")"
coder_id="$(python3 -c 'import tomllib,sys; print(tomllib.load(open(sys.argv[1], "rb"))["id"])' "$coder_source/FUNCTION.toml")"
planner_function="$(cd "$workspace" && XDG_DATA_HOME="$agl_home/data" "$forge_bin" path "$planner_id")"
coder_function="$(cd "$workspace" && XDG_DATA_HOME="$agl_home/data" "$forge_bin" path "$coder_id")"
[[ -n "$planner_function" && -n "$coder_function" ]] || die "Forge did not materialize both Functions"
export AGL_E2E_PLANNER_FUNCTION="$planner_function"
export AGL_E2E_CODER_FUNCTION="$coder_function"
cp --reflink=auto -- "$planner_model" "$agl_home/data/runtime/models/$planner_model_digest.gguf"
if [[ "$coder_model" != "$planner_model" || "$coder_model_digest" != "$planner_model_digest" ]]; then
  cp --reflink=auto -- "$coder_model" "$agl_home/data/runtime/models/$coder_model_digest.gguf"
fi

python3 - <<'PY'
import json, os
from pathlib import Path
config = Path(os.environ["AGL_E2E_AGL_HOME"]) / "config" / "agentLIBRE.toml"
config.write_text("[chat]\n"
                  "default_function = \"function:agentlibre.qwen38-27b-reasoning@^1.0\"\n\n"
                  "[inference]\n"
                  f"executable = {json.dumps(os.environ['AGL_E2E_ENGINE'])}\n\n"
                  "[integrations.search]\n"
                  "required = true\n"
                  "credential = \"ayeque-search\"\n\n"
                  "[credentials.ayeque-search]\n"
                  f"client_certificate = {json.dumps(os.environ['AGL_E2E_SEARCH_CERTIFICATE'])}\n"
                  f"client_private_key = {json.dumps(os.environ['AGL_E2E_SEARCH_PRIVATE_KEY'])}\n"
                  f"private_ca = {json.dumps(os.environ['AGL_E2E_SEARCH_CA'])}\n",
                  encoding="utf-8")
PY
AGL_HOME="$agl_home" "$execd_bin" >"$execd_log" 2>&1 &
execd_pid=$!
execd_socket="$agl_home/state/execd/execd.sock"
deadline=$((SECONDS + timeout_seconds))
until [[ -S "$execd_socket" ]]; do
  kill -0 "$execd_pid" 2>/dev/null || { tail -80 "$execd_log" >&2 || true; die "agl-execd exited"; }
  (( SECONDS < deadline )) || die "timed out waiting for agl-execd"
  sleep 0.2
done
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" config apply --function "$planner_function" --function "$coder_function") \
  >"$run_root/config-apply.out" 2>&1 || { sed -n '1,160p' "$run_root/config-apply.out" >&2; die "config apply failed"; }
AGL_HOME="$agl_home" "$agl_bin" serve >"$daemon_log" 2>&1 &
daemon_pid=$!
daemon_socket="$agl_home/state/daemon/agl.sock"
deadline=$((SECONDS + timeout_seconds))
until [[ -S "$daemon_socket" ]]; do
  kill -0 "$daemon_pid" 2>/dev/null || { tail -120 "$daemon_log" >&2 || true; die "daemon exited"; }
  (( SECONDS < deadline )) || die "timed out waiting for daemon"
  sleep 0.2
done

task_prompt='Create a complete implementation plan for the exact task in DECISIONS.md. The human decision record is authoritative: add shout(name) to hello.py returning greeting(name).upper(), add its regression test to test_hello.py, use exactly two sequential slices (implementation then test), and do not modify any other file. Cite the decision record and concrete source files. There are no unresolved product decisions. Emit only the plan object.'
single_prompt='Implement the exact task in DECISIONS.md directly in this repository. Read the decision record and source files, add shout(name) returning greeting(name).upper(), add its regression test, do not modify any other file, run pytest, and finish the implementation. This is the current single-loop baseline.'
single_started="$(date +%s)"
set +e
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" run "$coder_function" "$single_prompt") >"$run_root/single-loop.out" 2>&1
single_exit=$?
set -e
single_finished="$(date +%s)"
export AGL_E2E_SINGLE_EXIT="$single_exit"
python3 - <<'PY'
import os, subprocess
from pathlib import Path
root = Path(os.environ["AGL_E2E_WORKSPACE"])
subprocess.run(["git", "-C", str(root), "reset", "--hard", "HEAD"], check=True, stdout=subprocess.DEVNULL)
subprocess.run(["git", "-C", str(root), "clean", "-fd"], check=True, stdout=subprocess.DEVNULL)
PY

# Do not let the single-loop model service or its persisted retry health state
# affect the planner/coder profile comparison.
kill "$daemon_pid" 2>/dev/null || true
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
kill "$execd_pid" 2>/dev/null || true
wait "$execd_pid" 2>/dev/null || true
execd_pid=""
python3 - <<'PY'
from pathlib import Path
import os
for name in ("AGL_E2E_DAEMON_SOCKET", "AGL_E2E_EXECD_SOCKET"):
    path = os.environ.get(name)
    if path:
        Path(path).unlink(missing_ok=True)
PY
sleep 2
AGL_HOME="$agl_home" "$execd_bin" >"$execd_log" 2>&1 &
execd_pid=$!
execd_socket="$agl_home/state/execd/execd.sock"
deadline=$((SECONDS + timeout_seconds))
until [[ -S "$execd_socket" ]]; do
  kill -0 "$execd_pid" 2>/dev/null || { tail -80 "$execd_log" >&2 || true; die "agl-execd restart exited"; }
  (( SECONDS < deadline )) || die "timed out waiting for restarted agl-execd"
  sleep 0.2
done
AGL_HOME="$agl_home" "$agl_bin" serve >"$daemon_log" 2>&1 &
daemon_pid=$!
daemon_socket="$agl_home/state/daemon/agl.sock"
deadline=$((SECONDS + timeout_seconds))
until [[ -S "$daemon_socket" ]]; do
  kill -0 "$daemon_pid" 2>/dev/null || { tail -120 "$daemon_log" >&2 || true; die "daemon restart exited"; }
  (( SECONDS < deadline )) || die "timed out waiting for restarted daemon"
  sleep 0.2
done

(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" plan create --function "$planner_function" "$task_prompt") \
  >"$run_root/plan-create.json" 2>"$run_root/plan-create.stderr" || {
  tail -120 "$daemon_log" >&2 || true
  die "plan create failed"
}
plan_id="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["plan"]["id"])' "$run_root/plan-create.json")"
draft_digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["digest"])' "$run_root/plan-create.json")"
planner_conversation="$(sed -n 's/^conversation=//p' "$run_root/plan-create.stderr" | tail -1)"
[[ -n "$plan_id" && -n "$draft_digest" && -n "$planner_conversation" ]] || die "plan identity missing"
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" plan view "$plan_id") >"$run_root/plan-view.json"
python3 - <<'PY'
import json, os
from pathlib import Path
view = json.loads((Path(os.environ["AGL_E2E_RUN_ROOT"]) / "plan-view.json").read_text())
plan = view["plan"]
assert view["state"] == "ready_for_approval", view["state"]
assert plan["open_decisions"] == [], plan["open_decisions"]
assert plan["workspace"]["dirty_paths"] == [], plan["workspace"]["dirty_paths"]
assert len(plan["slices"]) == 2, "D3 requires two slices"
paths = {f["path"] for s in plan["slices"] for f in s["files"]}
assert paths == {"hello.py", "test_hello.py"}, paths
reads = {r["path"] for s in plan["slices"] for r in s["required_reads"]}
assert {"DECISIONS.md", "hello.py", "test_hello.py"} <= reads, reads
assert any(d["authority"] == "human" and "shout" in d["statement"]
           and any(x["locator"] == "DECISIONS.md" for x in d["sources"])
           for d in plan["decisions"])
assert any("pytest" in v["command"] for s in plan["slices"] for v in s["verification"])
assert any(any(x["locator"] == "DECISIONS.md" for x in e["sources"])
           for e in plan["evidence"])
PY

(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" plan approve "$plan_id" --digest "$draft_digest") >"$run_root/plan-approve.out"
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" plan view "$plan_id") >"$run_root/plan-approved-view.json"
approved_digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["digest"])' "$run_root/plan-approved-view.json")"
[[ "$approved_digest" == "$draft_digest" ]] || die "approval changed the digest"
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" plan implement "$plan_id" --digest "$approved_digest" --function "$coder_function") \
  >"$run_root/plan-implement.json" 2>"$run_root/plan-implement.stderr" || {
  tail -160 "$daemon_log" >&2 || true
  die "plan implement failed"
}
(cd "$workspace" && AGL_HOME="$agl_home" timeout "$timeout_seconds" \
  "$agl_bin" plan status "$plan_id") >"$run_root/plan-status.json"
implementation_finished="$(date +%s)"

python3 - <<'PY'
import json, os, subprocess
from pathlib import Path
root = Path(os.environ["AGL_E2E_RUN_ROOT"])
view = json.loads((root / "plan-status.json").read_text())
approved = json.loads((root / "plan-approved-view.json").read_text())
assert view["state"] == "completed", view["state"]
assert view["digest"] == approved["digest"]
plan, results = view["plan"], view["results"]
assert len(results) == len(plan["slices"]) == 2
assert all(r["state"] == "completed" for r in results), results
conversations = [r["conversation_id"] for r in results]
assert all(conversations) and len(set(conversations)) == len(conversations)
planner = (root / "plan-create.stderr").read_text().strip().split("conversation=")[-1]
assert planner not in conversations
assert all(r["plan_digest"] == view["digest"] for r in results)
by_id = {s["id"]: s for s in plan["slices"]}
for result in results:
    slice_ = by_id[result["slice_id"]]
    declared = {f["path"] for f in slice_["files"]}
    assert all(p["path"] in declared for p in result["changed_paths"])
    for dependency in slice_["depends_on"]:
        assert any(previous["slice_id"] == dependency for previous in results)
workspace = Path(os.environ["AGL_E2E_WORKSPACE"])
assert (workspace / "hello.py").read_text() == (
    "def greeting(name):\n    return f\"Hello, {name}!\"\n\n\n"
    "def shout(name):\n    return greeting(name).upper()\n")
subprocess.run(["python3", "-B", "-m", "pytest", "-q", "-p", "no:cacheprovider"], cwd=workspace, check=True)
dirty = subprocess.check_output(
    ["git", "-C", str(workspace), "status", "--short"], text=True)
assert {line[3:] for line in dirty.splitlines()} == {"hello.py", "test_hello.py"}, dirty
PY

python3 - <<'PY'
import json, os, sqlite3, subprocess
from datetime import date
from pathlib import Path
root = Path(os.environ["AGL_E2E_RUN_ROOT"])
workspace = Path(os.environ["AGL_E2E_WORKSPACE"])
db = Path(os.environ["AGL_E2E_AGL_HOME"]) / "data/store/agentlibre.sqlite3"
counts = {}
if db.exists():
    cx = sqlite3.connect(db)
    for table in ("agent_runs", "agent_operations", "agent_events", "agent_conversations"):
        try:
            counts[table] = cx.execute(f"select count(*) from {table}").fetchone()[0]
        except sqlite3.Error:
            counts[table] = None
    cx.close()
single = (root / "single-loop.out").read_text(errors="replace")
single_status = subprocess.check_output(
    ["git", "-C", str(workspace), "status", "--short"], text=True).strip().replace("\n", "; ")
view = json.loads((root / "plan-status.json").read_text())
results = view["results"]
planner = (root / "plan-create.stderr").read_text().strip().split("conversation=")[-1]
report = f"""# Planning implementation E2E

Date: {date.today().isoformat()}.
Status: real-model run completed by scripts/planning-implementation-live-e2e.sh.

## Environment

- Planner Function: {os.environ["AGL_E2E_PLANNER_FUNCTION"]}
- Coder Function: {os.environ["AGL_E2E_CODER_FUNCTION"]}
- Planner model: {os.environ["AGL_E2E_PLANNER_MODEL"]}
- Coder model: {os.environ["AGL_E2E_CODER_MODEL"]}
- Engine: {os.environ["AGL_E2E_ENGINE"]}
- Fixture commit: {subprocess.check_output(["git", "-C", str(workspace), "rev-parse", "HEAD"], text=True).strip()}

## Public workflow evidence

The harness executed plan create, view, explicit approve by digest, implement,
and status. The task added shout(name) and its regression test from the
four-item human decision record in DECISIONS.md.

- Plan ID: {view["plan"]["id"]}
- Planner Conversation: {planner}
- Draft/approved digest: {view["digest"]}; unchanged across the workflow
- Final state: {view["state"]}; slices: {len(results)}, all completed
- Coder Conversations: {", ".join(r["conversation_id"] for r in results)}; unique and distinct from planner
- Durable results: {", ".join(r["slice_id"] for r in results)}; each carries the approved digest
- Final verification: python3 -B -m pytest -q -p no:cacheprovider passed; only hello.py and test_hello.py were modified
- Plan checks: no open decisions; D1-D4, required reads, declared files and pytest verification were represented

## Same-task single-loop comparison

Before planning, the same coder Function received the same task through one
ordinary agl run. Exit status: {os.environ.get("AGL_E2E_SINGLE_EXIT", "captured by harness")}.
Output bytes: {len(single.encode())}. Workspace after that run: {single_status or "(clean)"}.
The fixture was restored to its committed baseline before plan create. Durable
store table counts after both runs: {counts}.

The historical failure in reports/compaction-reproduction-analysis.md records
96 read/search Tool calls, no patch, and limits_exceeded after 100 model calls.
This run measures progress: the planned flow completed two declared slices,
stored fresh Conversation IDs and durable results, and passed pytest.

Temporary runtime state and logs remain under {root}; they are not checked in.
"""
Path(os.environ["AGL_E2E_REPORT_PATH"]).write_text(report, encoding="utf-8")
PY

echo "planning E2E passed"
echo "report=$report_path"
echo "artifacts=$run_root"
