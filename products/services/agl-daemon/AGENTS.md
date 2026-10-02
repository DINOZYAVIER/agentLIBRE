# AGENTS.md

`agl-daemon` is the Agent service implementation and composition root. Keep
Agent scheduling under `agent`, durable repositories under `store`, and
native Tool composition under `tools`. Concrete first-party handlers belong
to `agl-tools`; the native SDK belongs to `agl-tool-api`.

Tool handlers must validate their own host safety boundary. Registering a
handler does not admit it to an AgentRun; admission remains tied to the exact
Function/Run snapshot and Effect grants.

Do not expose scheduler or store implementation through the public daemon API.
Do not create child processes or PTYs here; execution is delegated through
`agl-execution-api` to `agl-execd`.

## Module map (`src/`)

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Daemon configuration, listener lifecycle and service composition; hosted by `agl serve`. |
| `protocol.rs` | Public command dispatch, response conversion and subscriptions. |
| `activation.rs` | Function activation/cache integration with Agent admission and inference health. |
| `agent/service.rs`, `agent/scheduler.rs` | Agent service handles, bounded scheduler, cancellation and recovery. |
| `agent/run_driver.rs`, `agent/operation_driver.rs` | Run admission/snapshots, Run driving and model/Tool operation execution. |
| `agent/common.rs`, `agent/progress.rs` | Shared errors/time helpers and progress delivery. |
| `agent/compaction.rs` | Context compaction and memory-extraction processing. |
| `agent/memory_io.rs` | Contained memory reads and guarded corpus writes. |
| `agent/memory_index.rs` | Memory search/index implementation; currently not declared in `agent/mod.rs`. |
| `planner.rs` | Planner policy, instruction construction and implementation-plan response schema. |
| `plans.rs` | Plan artifact files and create/view/approve coordination. |
| `implementation.rs` | Slice execution, verification, changed-path checks and post-verification results. |
| `workspace.rs` | Workspace snapshots/fingerprints, executable-bit tracking and `.git`/output-directory exclusion. |
| `store/` | SQLite connection/ownership/rotation, Agent transitions/queries/compaction, plan state and inference health persistence. |
| `tools/mod.rs` | Composition/re-exports of native providers from `agl-tools`. |

`lib_tests.rs` and `agent/*tests.rs` cover service behavior; Store tests live
under `store/agent/tests.rs`.
