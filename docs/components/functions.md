# Functions

agentFUNCTIONs are user-facing agent setup artifacts. They bind an agent's
system prompt, inference config, skills, tools, memory policy, and subagent
artifacts into one inspectable object.

Status: CLI MVP implemented; subagent execution and profile-manager UI remain
future work.

## What Exists

Create and inspect function artifacts:

```bash
agl function init coding --workspace
agl function list
agl function show coding
agl function status coding
agl function doctor coding
```

Run or chat with a function:

```bash
agl run --function coding --prompt "Summarize this repo."
agl chat --function coding
```

The MVP reads `FUNCTION.md`, reads the required sibling `SYSTEM.md`, resolves
`model.config` or `model.profile`, injects the function system prompt into
model context, merges function skills with config `[prompt].skills` and CLI
`--skill`, and writes run evidence:

- `function-resolution.json`
- `function-context.md`
- `subagent-registry.json`

Inside chat, `/reload` refreshes the function manifest, system prompt, selected
skills, visible tools, and subagent registry. Changing `model.config`,
`model.profile`, `--config`, model paths, or backend settings still requires a
new chat or run.

## Goals

- Give users one understandable object to choose before `agl run` or
  `agl chat`.
- Keep low-level runtime and inference details in TOML configs while letting
  humans and LLMs maintain the higher-level system prompt in Markdown.
- Make skills, subagents, tools, memory, and profile selection visible through
  status commands before a run starts.
- Support a small CLI-first MVP that a later TUI/profile manager can reuse.
- Preserve clear reload semantics: prompt context can reload inside chat, model
  process settings require a new run or chat.

## Non-Goals

- Do not invent a custom DSL or custom file extension for the MVP.
- Do not port the legacy profile UI code directly. The legacy workspace is a UX
  reference, not an implementation source.
- Do not make subagents top-level profiles. Subagents are Markdown artifacts
  referenced by a function.
- Do not add daemon-managed model hot swapping in the MVP.

## Vocabulary

- `agentFUNCTION`: the persisted user-facing object. It answers "what should
  this agent be and what can it use?".
- `Agent`: a runtime instance of an agentFUNCTION with a resolved inference
  profile, workspace, permissions, and turn state.
- `Subagent`: a callable Markdown artifact declared by an agentFUNCTION.
- `Skill`: a trusted instruction pack managed by the skills subsystem.
- `Tool`: a runtime operation exposed under a tool access mode.
- `Inference config`: a TOML model/runtime config owned by a function or
  selected through a named profile override.

## Storage

Use one YAML-front-matter manifest and one Markdown system prompt file:

```text
.agl/
  functions/
    coding/
      FUNCTION.md
      SYSTEM.md
      inference.toml
      subagents/
        reviewer.md
        tester.md
```

Global functions live under the AgentLIBRE config directory:

```text
$AGL_HOME/config/functions/<id>/FUNCTION.md
$AGL_HOME/config/functions/<id>/SYSTEM.md
$AGL_HOME/config/functions/<id>/inference.toml
```

Workspace functions live under the repository workspace:

```text
.agl/functions/<id>/FUNCTION.md
.agl/functions/<id>/SYSTEM.md
.agl/functions/<id>/inference.toml
```

Resolution order:

1. Explicit path supplied on the CLI.
2. Workspace function by id.
3. Global function by id.
4. Builtin function asset by id.

Builtin functions are packaged under `assets/functions/<id>/` and embedded into
the binary by `agl-assets`.

Function and subagent ids must be lowercase ASCII with letters, digits,
hyphen, underscore, or dot. File resolution must reject path traversal.

## Function Format

`FUNCTION.md` is the machine-readable manifest. It uses YAML front matter only;
Markdown body content is invalid. The function system prompt is always the
required sibling file `SYSTEM.md`. The manifest does not name that file.

```markdown
---
schema: agentfunction/v1
id: coding
title: Coding
description: Local coding agent with review and test helpers.
model:
  config: inference.toml
runtime:
  tool_mode: write
  max_output_tokens: 512
skills:
  use:
    - repo-status
    - coding-style
tools:
  allow:
    - fs.read
    - fs.write
    - shell.exec
  deny:
    - network.open
subagents:
  use:
    - reviewer
    - tester
memory:
  read:
    - user
    - repo
  write:
    - repo
artifacts:
  keep_runs: true
doctor:
  smoke_prompt: "Inspect the repository and report whether tests are visible."
contracts:
  identity:
    mode: validate_claims
    fields:
      - function
      - skills
      - subagents
    repair: true
    max_repair_attempts: 1
---
```

`SYSTEM.md`:

```markdown
You are the `coding` agentFUNCTION.

Maintain this repository through small, reviewable changes.

Inspect the repo before editing.
Prefer existing project conventions over new abstractions.

Use `reviewer` before finalizing broad code changes.

If a skill is unavailable, run `agl skill inspect <name> --runtime`.
```

Required front matter fields:

- `schema`: must be `agentfunction/v1`.
- `id`: stable function id.
- `title`: short display name.

Recommended front matter fields:

- `description`: one-sentence summary.
- `model.config`: relative path to a function-owned local inference TOML.
- `model.profile`: named inference profile. `model.config` and
  `model.profile` are mutually exclusive.
- `runtime.tool_mode`: one of `read-only`, `write`, `execute`, `approve`,
  or `admin`.
- `runtime.max_output_tokens`: default answer budget for run/chat.
- `skills.use`: skill ids to inject.
- `subagents.use`: subagent ids under `subagents/<id>.md`.
- `memory.read` and `memory.write`: memory scopes the function may use.
- `doctor.smoke_prompt`: optional smoke prompt for `agl function doctor`.
- `contracts.identity`: optional runtime identity validation policy; see
  [Hooks](hooks.md).

Unknown front matter fields should fail validation unless the key starts with
`x-`. This keeps the format strict while leaving an extension lane. `prompt`
is not a function manifest field; use the sibling `SYSTEM.md` file.

`SYSTEM.md` is regular Markdown. The renderer injects it as the function's
system prompt context. It must sit next to `FUNCTION.md` and must not be empty.

`inference.toml` is a normal local inference config for model and runtime
settings. Config-level `[prompt]` settings are not the function system prompt;
they must not point to `SYSTEM.md`. Packaged function configs set
`[prompt].system = "none"` so `SYSTEM.md` is the only function-owned system
prompt.

## Subagent Format

Each subagent is a Markdown artifact under the owning function directory:

```markdown
---
schema: agentlibre/subagent/v1
id: reviewer
title: Reviewer
model:
  inherit: true
tools:
  mode: read-only
skills:
  use:
    - repo-status
limits:
  max_turns: 3
  max_output_tokens: 512
---

# Mission

Review the proposed change for correctness, regression risk, and missing tests.

# Operating Rules

- Lead with concrete findings.
- Reference files and line numbers when possible.

# Handoff

Return a concise review summary to the parent agent.
```

Subagent ids are local to the owning function. The file name and front matter id
must match. A subagent may inherit the parent model or select a different
profile, but profile switching is only a declaration in the MVP unless the
runtime supports spawning that process.

## Inference Configs

Function-owned inference configs remain TOML. `model.config` points to a TOML
file inside the function directory, and builtin functions embed that file as an
asset. This is the preferred shape for packaged model/runtime selections.

Named inference profiles still exist for manual overrides and local
experiments:

```text
$AGL_HOME/config/inference/local.toml
```

The profile manager should add named profiles without breaking that path:

```text
$AGL_HOME/config/inference/profiles/<id>.toml
.agl/inference/profiles/<id>.toml
```

Resolution for `model.config`:

1. Relative path inside the resolved function directory.
2. Embedded asset for builtin functions.

Resolution for `model.profile`:

1. Workspace profile by id.
2. Global named profile by id.
3. `local` maps to the existing local inference config.

`model.config` and `model.profile` are mutually exclusive. `--config PATH` on
`agl run` or `agl chat` overrides either one for that invocation. It does not
disable the function's skills, subagents, or system prompt context.

Current builtin model functions:

- `gemma4-12b`: packaged Gemma4 12B QAT config.
- `gemma4-26b`: packaged Gemma4 26B-A4B QAT config.
- `gemma4-31b`: packaged Gemma4 31B QAT config.

## Runtime Semantics

`agl run --function <id>` and `agl chat --function <id>` should:

1. Resolve the function.
2. Parse and validate front matter.
3. Read and validate the required system prompt file.
4. Resolve the function-owned inference config or named inference profile.
5. Resolve selected skills.
6. Resolve selected subagent artifacts.
7. Render deterministic function context for the model.
8. Add runtime identity validation hooks when `contracts.identity` enables
   them.
9. Write resolution evidence into the run artifact directory.

CLI flags override or extend function defaults:

- Scalar flags such as `--tool-mode`, `--config`, and
  `--max-output-tokens` override the function.
- List flags such as `--skill` append to function lists and de-duplicate by id.
- Explicit deny rules remain deny rules even if another source allows the same
  tool.

Inside `agl chat`, `/reload` should refresh the function manifest, system
prompt, selected skills, visible tools, and subagent registry. Changing the
selected inference config/profile, model path, backend runtime settings, or
`--config` requires starting a new chat or run.

If `/reload` changes the effective runtime identity, the next turn should use
the refreshed `function-resolution.json`, `subagent-registry.json`, and
identity hook payload. Profile changes still require a new chat or run.

Subagents are advertised to the parent agent as available helpers. Actual
subagent execution can be implemented after the MVP parser and context path are
stable.

## CLI MVP

Add a top-level command:

```bash
agl function list [--json]
agl function show <id-or-path> [--json]
agl function status <id-or-path> [--strict] [--json]
agl function init <id> [--workspace] [--model-profile NAME]
agl function doctor <id-or-path> [--json]
```

Add function selection to existing runtime commands:

```bash
agl run --function coding --prompt "Summarize this repo."
agl chat --function coding
```

`list` should show id, title, source, path, and validation status.
`show` should render resolved front matter, the conventional `SYSTEM.md`
path/content, and referenced subagents.
`status` should validate references without invoking inference.
`doctor` should run the optional smoke prompt and write evidence.
`init` should create a minimal `FUNCTION.md`, `SYSTEM.md`, and a `subagents/`
directory.

## Evidence And Repair

Every run/chat started with a function should emit these artifacts:

- `function-resolution.json`: selected function path, source, inference config
  path, `SYSTEM.md` path, override flags, and validation status.
- `function-context.md`: rendered context that was sent to inference.
- `subagent-registry.json`: selected subagent ids, paths, titles, and status.
- `runtime-identity.json`: exact function, profile, skill, subagent, workspace,
  and tool-mode identity used by identity hooks.
- `identity-contract.json`: effective identity validation and repair policy
  after function defaults and CLI overrides.

`agl function status` should print repair hints for:

- Missing or invalid function file.
- Missing inference config or referenced inference profile.
- Missing, untrusted, or unusable skills.
- Missing subagent files.
- Front matter parse errors with line and column where available.
- Tool names or modes that are not known to the runtime.

`agl config status` should remain the entry point for global paths and logs.
Function status output should point back to:

- `app_log`
- `inference_log`
- `sessions_root`
- run artifact root
- skill trust store

## Crate Boundaries

Add a new `agl-functions` crate for the component core:

- schema structs
- Markdown/front matter parser
- path resolver
- registry and source classification
- validation report model
- context renderer
- test fixtures

Keep command wiring in `agl-cli`.

Keep chat/run integration in `agl-chat` and existing runtime paths. The chat
layer should receive a resolved function context rather than parsing files
itself.

Use `agl-runtime` only for shared path helpers and artifact/evidence plumbing.

Do not fold this into `agl-config`: functions are behavioral artifacts, not
only low-level configuration.

## Profile Manager Integration

The profile manager should expose reusable view models for:

- Function-owned inference configs and named inference profiles.
- Functions that reference those profiles.
- Skills and subagents selected by each function.
- Validation state and repair hints.

The legacy profile screen is useful as a UX reference: a list panel, a details
panel, and an inventory/status panel still fit. The implementation should be
fresh and backed by the `agl-functions` and inference config/profile APIs so
CLI and TUI see the same state.

## Validation Rules

The MVP validator should reject:

- Unsupported `schema`.
- Invalid ids.
- Function id that does not match its directory id, unless loaded by explicit
  file path.
- Missing or empty system prompt file.
- Markdown body content in `FUNCTION.md`.
- Subagent id that does not match the file stem.
- Relative paths that escape the function directory.
- Unknown front matter fields without an `x-` prefix.
- Invalid `runtime.tool_mode`.
- Missing referenced skills in strict mode.
- Missing function-owned inference config or referenced inference profile.

Non-strict mode may warn for missing optional references, but it should still
fail malformed front matter and unsafe paths.

## Test Plan

- Parser fixtures for valid function and subagent files.
- Parser fixtures for invalid YAML, unknown keys, unsupported schema, and path
  traversal.
- Loader fixtures for rejected `prompt` fields, missing `SYSTEM.md`, empty
  `SYSTEM.md`, and body content in `FUNCTION.md`.
- Resolution tests for explicit path, workspace function, global function, and
  `local` profile fallback.
- CLI tests for `function list`, `show`, `status`, and `init`.
- Snapshot or golden tests for rendered function context.
- Chat tests proving `/reload` refreshes function context without recreating
  the model session.
- Run/chat tests proving `--config` overrides `model.config` or
  `model.profile` while preserving function skills and system prompt context.

## Implementation Phases

1. Add this spec and keep product vocabulary stable.
2. Add `agl-functions` with parser, schema, registry, validator, renderer, and
   fixtures.
3. Add `agl function list/show/status/init` and JSON output.
4. Add `--function` to `agl run` and `agl chat`; emit resolution evidence.
5. Wire `/reload` to refresh function context and subagent registry.
6. Add `agl function doctor`.
7. Add named inference profile manager commands.
8. Build TUI/profile-manager views on the same APIs.
9. Add real subagent invocation after function loading is observable and
   stable.
