# AGENTS.md

`agl-daemon` is the Agent service implementation and composition root. Keep
Agent scheduling under `agent`, durable repositories under `store`, and
native Tool composition under `tools`. Concrete first-party handlers belong
to `agl-tools`; the native SDK belongs to `agl-tool-api`.

Tool handlers must validate their own host safety boundary. Registering a
handler does not admit it to an AgentRun; admission remains tied to the exact
Function/Run snapshot and Effect grants.

Do not expose scheduler or store implementation through the public daemon API.
Keep execution bindings under `execution/`: systemd owns process groups,
tmux owns terminals, and Store owns reservations/results. Preserve planner
and private inference isolation in the execution helper.

## Module map (`src/`)

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Daemon configuration, listener lifecycle and service composition; hosted by `agl serve`. |
| `protocol.rs` | Public command dispatch, response conversion and subscriptions. |
| `activation.rs` | Function activation/cache integration with Agent admission and inference health. |
| `agent/schedules.rs`, `agent/schedule_events.rs` | Autonomous timer/file triggers, fixed enable grid and event suppression. |
| `agent/service.rs`, `agent/scheduler.rs` | Agent service handles, bounded scheduler, cancellation and recovery. |
| `agent/run_driver.rs`, `agent/operation_driver.rs` | Run admission/snapshots, Run driving and model/Tool operation execution. |
| `agent/common.rs`, `agent/progress.rs` | Shared errors/time helpers and progress delivery. |
| `agent/compaction.rs` | Context compaction and memory-extraction processing. |
| `agent/memory_io.rs` | Contained citation reads and pure corpus write plans. |
| `agent/memory_publication.rs` | Source Git commits, Run/Archive journal recovery, revision update and Forge sync. |
| `agent/memory_worker.rs` | One bounded terminal-publication worker outside active Run slots; pending claims/journals stay in the existing Store/files. |
| `agent/memory_index.rs` | Disposable FTS5 projection bound to a Forge source under its read guard. |
| `planner.rs` | Planner policy, instruction construction and implementation-plan response schema. |
| `plans.rs` | Plan artifact files and create/view/approve coordination. |
| `implementation.rs` | Slice execution, verification, changed-path checks and post-verification results. |
| `workspace.rs` | Workspace snapshots/fingerprints, executable-bit tracking and `.git`/output-directory exclusion. |
| `execution/` | Pure execution FSM, systemd/tmux adapter, bounded capture helpers and planner/private inference isolation. |
| `store/` | SQLite connection/ownership/rotation, Agent transitions/queries/compaction, plan state and inference health persistence. |
| `tools/mod.rs` | Composition/re-exports of native providers from `agl-tools`. |
| `tools/memory.rs` | Memory search host, Run-pinned tokenizer and complete-result budget. |

`lib_tests.rs` and `agent/*tests.rs` cover service behavior; Store tests live
under `store/agent/tests.rs`; schedule persistence is in `store/agent/schedules.rs`.
`src/schedule_protocol_tests.rs` checks scheduling through the public Unix protocol
in an isolated child process; Forge is a test-only dependency of the daemon.
