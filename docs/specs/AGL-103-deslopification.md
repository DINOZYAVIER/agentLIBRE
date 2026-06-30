# AGL-103 Deslopification Modular Refactor

## Goal

Reduce large catch-all Rust files by moving cohesive code into named modules,
without changing runtime behavior, public CLI contracts, persistence schemas, or trust semantics.

## Rules

- Prefer mechanical extraction before semantic cleanup.
- Keep public and `pub(crate)` API surfaces stable unless a follow-up spec says otherwise.
- Commit each coherent slice separately.
- Use focused tests and clippy for each slice; run full workspace checks before closing a wave.
- Do not hide behavior changes inside refactor commits.
- Keep `lib.rs` files as API/module facades when possible.

## Current Status

Implemented slices:

1. `agl-store/src/migrations.rs`: store schema migrations.
2. `agl-cli/src/store.rs`: `agl store ...` command handling.
3. `agl-cli/src/args/model.rs`: CLI command and option model.
4. `agl-cli/src/args/tests.rs`: CLI parser tests.
5. `agl-store/src/error.rs`: store error/result type.
6. `agl-store/src/types.rs`: store DTO/status model.
7. `agl-cli/src/repo.rs`: repo workspace command handling and repo-specific output.
8. `agl-store/src/tests.rs`: store tests.
9. `agl-cli/src/memory.rs`: memory command handling and output.
10. `agl-cli/src/notes.rs`: notes command handling and output.

## Refactor Inventory

### Priority 1

`crates/agl-repo/src/lib.rs`

- Current issue: workspace DTOs, profile import/export, manifest rendering, git discovery/status,
  hook installation, path validation, and tests live in one file.
- Target modules:
  - `types.rs`: public option/report/status/profile/component structs and enums.
  - `manifest.rs`: manifest defaults, read/write, profile conversion, validation.
  - `git.rs`: repo root discovery, git command wrappers, submodule/gitlink status.
  - `hooks.rs`: hook planning, rendering, installation, executable permission helpers.
  - `status.rs`: component status and workspace status assembly.
  - `tests.rs`: current unit tests.
- First safe slices: move tests, then types, then hooks.

`crates/agl-chat/src/session.rs`

- Current issue: session lifecycle, inference request assembly, runtime capability rendering,
  memory context, skill context, dynamic permission grants, evidence writing, and tests are mixed.
- Target modules:
  - `request.rs`: `build_inference_request` and request assembly helpers.
  - `capabilities.rs`: runtime capability block and tool context rendering.
  - `memory_context.rs`: memory context policy/render/evidence.
  - `skill_context.rs`: skill selection, hook batches, visible tools, dynamic grants.
  - `evidence.rs`: evidence file writers.
  - `tests.rs`: current tests.
- First safe slices: move tests, then capability rendering, then context resolution.

`crates/agl-cli/src/args.rs`

- Current issue: clap DTOs, parse entrypoint, command mapping, validators, completion facade,
  and hidden/retired command handling remain together.
- Target modules:
  - `args/parser.rs`: `parse_cli`, `Cli`, top-level command conversion.
  - `args/clap.rs`: clap DTO structs/enums.
  - `args/map.rs`: conversion from clap DTOs to command model.
  - `args/validation.rs`: prompt, limit, confidence, skill id validation.
  - `args/completion.rs`: public completion facade.
- First safe slices: move validators, then completion facade.

`crates/agl-skills/src/workspace.rs`

- Current issue: workspace discovery, lockfile construction, trust store, diagnostics, duplicate
  detection, builtin shadowing, and tests are mixed.
- Target modules:
  - `workspace/types.rs`: public report/status/lock/trust DTOs.
  - `workspace/discovery.rs`: workspace skill discovery and manifest collection.
  - `workspace/diagnostics.rs`: duplicate/shadow/broadening diagnostics and next steps.
  - `workspace/lock.rs`: lockfile read/write/build.
  - `workspace/trust.rs`: trust store read/write/apply/trust/revoke.
  - `workspace/tests.rs`: current tests.
- First safe slices: move tests, then trust store helpers.

### Priority 2

`crates/agl-cli/src/lib.rs`

- Current issue: top-level dispatch, cron handlers, skill handlers, daemon status, chat/serve,
  and shared output formatting remain in one file.
- Target modules:
  - `cron.rs`: cron command handling and cron-specific output.
  - `skill.rs`: skill command handling and skill-specific output.
  - `daemon.rs`: daemon status command.
  - `serve.rs`: serve command options/run.
  - `infer.rs`: one-shot inference command.
  - Keep only CLI bootstrap, runtime resolution, process mode, and shared tiny helpers in `lib.rs`.
- First safe slices: daemon status, then skill handlers, then cron.

`crates/agl-store/src/lib.rs`

- Current issue: connection lifecycle, migration execution, domain health, idempotency,
  matrix notification outbox, permission requests/grants, JSONL export, path validation,
  and row mappers remain together.
- Target modules:
  - `connection.rs`: open/migrate/schema status/health.
  - `idempotency.rs`: idempotency methods and mappers.
  - `matrix_outbox.rs`: Matrix notification outbox methods and mappers.
  - `permissions.rs`: permission request/grant methods and mappers.
  - `export.rs`: JSONL export and domain counts.
  - `path.rs`: store root/database path/private permissions.
- First safe slices: export helpers, then path helpers.

`crates/agl-cron/src/lib.rs`

- Current issue: error/model types, repository persistence, validation, schedule parsing/matching,
  idempotency keying, row mappers, time math, and tests are mixed.
- Target modules:
  - `types.rs`: job/run/draft/update/due/admission model.
  - `error.rs`: cron error/result.
  - `repo.rs`: `CronRepository` persistence methods.
  - `schedule.rs`: expression validation and matching.
  - `time.rs`: timezone parsing and civil-time math.
  - `tests.rs`: current tests.
- First safe slices: move tests, then schedule/time.

`crates/agl-matrix-bridge/src/runtime.rs`

- Current issue: runtime object, login/session storage, sync loop, verification/SAS,
  outbox delivery, reply parsing, event conversion, and tests are mixed.
- Target modules:
  - `runtime/types.rs`: login, device, outbox, verification DTOs.
  - `runtime/session.rs`: Matrix session load/save/validation/store paths.
  - `runtime/verification.rs`: verification request/SAS presentation.
  - `runtime/outbox.rs`: daemon-owned Matrix outbox delivery.
  - `runtime/events.rs`: inbound event conversion/reply context/relation.
  - `runtime/tests.rs`: current tests.
- First safe slices: move session helpers, then event helpers.

### Priority 3

`crates/agl-memory/src/lib.rs`

- Target modules: `error.rs`, `types.rs`, `repo.rs`, `row.rs`, `validation.rs`, `tests.rs`.

`crates/agl-skills/src/manifest.rs`

- Target modules: schema/types, parsing, permission policy validation, route/tool validation,
  diagnostics, tests.

`crates/agl-inference/src/llama_cpp/session.rs`

- Target modules: process spawning, prompt/session IO, event parsing, lifecycle cleanup, tests.

`crates/agl-matrix-bridge/src/main.rs`

- Target modules: CLI args, command dispatch, output rendering, runtime wiring.

`crates/agl-events/src/event.rs`

- Target modules: event types, serialization helpers, validation/tests if present.

`crates/agl-chat/src/service.rs`

- Target modules: service options/state, turn execution, persistence integration.

`crates/agl-tools/src/cron.rs`, `fs.rs`, `memory.rs`, `notes.rs`, `permissions.rs`,
`registry.rs`, `guards/validators.rs`

- Current issue: each is below the first wave threshold but still large enough for later
  domain-focused splits.
- Target: split only after host-tool boundary stabilizes, so refactors do not churn tool APIs twice.

## Current Wave Plan

1. `agl-repo`: split tests out of `lib.rs`.
2. `agl-repo`: split public types out of `lib.rs`.
3. `agl-repo`: split hook planning/installing into `hooks.rs`.
4. Run focused `agl-repo` tests/clippy after every slice.
5. Run full workspace checks after the wave.
