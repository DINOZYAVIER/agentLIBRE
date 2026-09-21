# Daemon

`products/services/agl-daemon` is the composition root for the local Agent
process. It opens the single SQLite `StoreHandle`, restores inference health,
starts `RuntimeService`, then starts `AgentService` with the runtime-backed
inference route. A prompt creates an `AgentRun`; it does not reconstruct these
services.

The daemon routes the bounded protocol directly to exact handles:

- Function/workspace activation → `RuntimeHandle::activate`, followed by one
  immutable Conversation binding;
- `StartRun` → `AgentHandle::start_run` using that frozen binding;
- `CancelRun` → `AgentHandle::cancel`;
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
serves a same-UID Unix socket with JSONL frames bounded to 1 MiB. Admitted
execution Tools are translated to `agl-execution-api` requests; the daemon
does not create their processes or PTYs.

Configured integrations carry an explicit `required` value. An invalid optional
search binding starts the daemon in degraded health with search disabled. An
invalid required search binding is a permanent startup fault and exits with the
configuration-error status instead of entering a restart loop.
