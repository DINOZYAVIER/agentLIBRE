# Daemon

The Linux user daemon is the long-running owner of interactive sessions,
application projections, turn execution, persistent terminals, and background
operations. Bare `agl` is a presentation client; it does not fall back to an
in-process interactive chat or launch a Human shell itself.

The first interactive transport is a private Unix socket. The server validates
the peer UID, enforces bounded protocol v5alpha frames/streams, and supports
systemd user socket activation. A manual bind requires an absolute canonical
parent owned by the daemon UID with mode `0700`, rejects symlinked or public
custom parents, and verifies the bound socket is owned with mode `0600`.
`agl-app` owns the presentation-neutral command catalog and session projection,
while `agl-process` remains the sole OS process/PTY owner. This is also the
boundary a future native GUI will use.

Semantic prompt submission also crosses `agl-app`: `RunSubmit` is admitted
against the authoritative application snapshot before the daemon starts or
queues the turn. Subscribers therefore observe one ordered projection with
typed queued, active and finished transitions rather than a second daemon-only
view of prompt state.

Assistant text deltas and model/tool lifecycle updates are volatile private
presentation events. They never enter safe runtime events or evidence; the
durable final message reconciles the same provisional message identity. Slow
or discontinuous subscribers receive a typed resync requirement rather than
blocking inference.

Presentation transcript pages are bounded to 2,000 items and 8 MiB decoded
JSON. The daemon sends each logical page as an ordered manifest/chunk/finish
transfer whose individual JSONL frames stay below 1 MiB; the client verifies
the declared identity, revision, lengths, item count and SHA-256 digest before
installing it. Older-page cursors are opaque and scoped to one session and
daemon epoch. Older pages are scanned backwards from their byte cursor in
bounded blocks and records; serving a page never loads the whole transcript
into memory. Fetching older history does not mutate the current live projection.

Each durable session serializes root prompts through a bounded queue. Human
terminal input remains concurrent because it belongs to the persistent PTY,
not to the model-turn concurrency key. `/disconnect` closes only the client
surface. Confirmed `/exit` cancels active and queued root turns, terminates
session work, and finishes the durable session.

Mutable daemon application state has one bounded owner executor. Connection
tasks enqueue synchronous owner operations without holding a shared mutex;
disconnect cancellation propagates into queued or running bridge work, and
the bounded permit remains charged until that work actually stops. Session
exit prepares the cancellation under the owner, waits for run termination
outside it, then finalizes the durable session so unrelated requests remain
responsive.

Human Host terminal creation is a separate same-UID local-operator operation,
not a model capability grant. It requires an explicit confirmation, creates a
terminal-lifetime authority record, and cannot be converted into agent Host
authority. Capability-grant reconciliation therefore cannot revoke a live
operator terminal accidentally; session exit and explicit terminal kill still
terminate it. Disconnecting the confirming client does not end that terminal.

The daemon persists terminal identity and immutable admission metadata before
the backing execution is spawned. On a new daemon epoch, prior live terminal
records and executions become `outcome_unknown`; clients must resnapshot and
the daemon never replays shell input or starts a replacement process
automatically.
