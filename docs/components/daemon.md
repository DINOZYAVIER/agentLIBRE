# Daemon

`products/services/agl-daemon` is the composition root for the local Agent
process. It opens the single SQLite `StoreHandle`, restores inference health,
starts `RuntimeService`, then starts `AgentService` with the runtime-backed
inference route and the schedule trigger service. A prompt creates an `AgentRun`; it does not reconstruct these
services.

The daemon routes the bounded protocol directly to exact handles:

- Function/workspace activation → `RuntimeHandle::activate`, followed by one
  immutable Conversation binding;
- Schedule commands → persistent enable/disable state, frozen Conversation
  snapshots, timer/file triggers and Run cancellation;
- `StartRun` → `AgentHandle::start_run` using that frozen binding;
- `CancelRun` → `AgentHandle::cancel` and execution cancellation for that Run;
- `RunView`, `Events`, and `Messages` → typed `StoreHandle` queries; and
- `Subscribe` → durable Store events plus transient `AgentProgress`.

There is no application facade or authoritative presentation reducer. The
client owns cursor/resynchronization and rendering. Subscription
setup registers transient delivery before reading the Store cursor, suppresses
already-committed buffered events, and emits `Lagged` when transient delivery
overflows.

Inference runs behind `RuntimeHandle`. The runtime may acquire locked bytes,
load, reuse, evict, or queue a compatible private `llama-server` realization
without changing the Run's identity. Function or Matrix configuration changes
affect only newly created Conversations.

The private daemon store opens the current alpha SQLite baseline. There is no startup path
that deletes legacy runtime directories or migrates older schemas.

The public transport and async client live in `agl-daemon-api`. The daemon
serves a same-UID Unix socket with JSONL frames bounded to 8 MiB. Admitted
execution Tools invoke the daemon execution adapter. The Store owns durable
execution bindings/results; systemd owns process groups and tmux owns PTYs.

Planning/implementation workspace snapshots hash file contents, entry types,
and symlink targets outside explicitly declared `output_directories` in the
root workspace manifest. Build-output changes alone do not invalidate a plan;
new or changed project files outside the current slice still do. The manifest
and parsed output scope participate in the snapshot digest. The executable bit participates in the fingerprint; other permission bits
and timestamps do not.

Configured integrations carry an explicit `required` value. An invalid optional
search binding starts the daemon in degraded health with search disabled. An
invalid required search binding is a permanent startup fault and exits with the
configuration-error status instead of entering a restart loop.
