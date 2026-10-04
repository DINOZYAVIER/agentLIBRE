# Agents

One admitted prompt is one durable `AgentRun`. A `Conversation` freezes one
exact Function snapshot and workspace; later Runs in it reuse that binding
without rereading mutable Function files.

## Durable Agent state

`agl-core` owns the neutral data and two pure state machines:

```text
AgentRun
  └── AgentFsm
       └── AgentOperation
            └── AgentOperationFsm
                 └── model or Tool adapter
```

`AgentFsm` owns Run lifecycle, the Ready/Waiting checkpoint, local usage, and
the model/Tool loop. `AgentOperationFsm` owns delivery, bounded retry,
recovery, cancellation, and `OutcomeUnknown`. Neither FSM performs I/O or
allocates durable IDs.

The private `agl-daemon::store` modules atomically persist accepted state, the optional message, and the
ordered structural `AgentEvent` list. Assistant and Tool content is stored
exactly once on `AgentMessage`; the producing Operation references that
message. Conversation queries return only `MessageVisibility::Conversation`.
When a Function enables preserved reasoning, private reasoning is stored only
inside operation metadata and is materialized solely for later inference
context. It is never returned by message, CLI, or Matrix queries.

Before each generation or compaction inference attempt, `AgentFsm` reserves
one `model_calls` unit; output correction reserves `correction_calls` instead.
Store commits `ModelCallStarted` and the updated usage before inference dispatch.
An interrupted attempt retains that charge across restart, including transport
retries. Measurement/tokenization do not consume a call. Completion adds known
token usage without charging calls again; an exhausted call budget fails before
inference dispatch. Token usage for an interrupted attempt without a committed
result remains unknown.

## Immutable admission

`AgentRunSnapshot` freezes exactly:

- exact Agent and model package identity;
- ordered materialized instructions;
- canonical workspace root and relative working directory;
- admitted Tool definitions with Extension/package provenance and digests;
- scoped authority grants; and
- finite local limits.

Recovery reads this snapshot, the ordered message context, checkpoint,
Operations, and Store journal. It does not reread mutable package files or
re-resolve the current default model.

## Runtime ownership

Private `agl-daemon::agent` modules own the long-lived service, cloneable
handle, bounded scheduler, FSM driver, prepared typed Extension bindings,
Tool execution, and transient progress. Missing, duplicate, or stale bindings
fail before Run admission.

`agl-runtime` activates a locked Function and supplies the frozen snapshot.
`agl-runtime::inference` owns shared model services, bounded slots and request
correlation. Process lifecycle is delegated to systemd through the daemon adapter. Every result
records the actual runtime-profile, engine-build and physical-resource
digests. Worker health and resource quarantine are the only durable inference
safety records.

Model requests admit one Tool call at a time (`parallel_tool_calls` is false).
For the read-only filesystem Tools, the driver compares each request with the
immediately preceding durable Tool operation. The first exact duplicate is not
executed: it produces a compact correction containing the prior operation and
available `next_cursor`. Repeating that exact request once more fails the
operation and Run with `tool_loop_detected`. A changed request resets the
sequence, and recovery derives the same decision from the Store rather than
process memory.

When a Run reaches a terminal state, the scheduler reloads its durable view and
logs the status, failure kind, input and output tokens, model calls, and Tool
calls.

Terminal memory publication runs on one separate worker in the same daemon,
after the scheduler releases the Run slot. Its nonblocking queue holds up to
256 Run IDs. Overflow records `MemoryPublicationDeferred`; pending claims and
publication journals remain for a later terminal Run. Shutdown stops admission,
discards queued notifications and waits only for an already-started publication
within its configured sync deadline. Run status does not depend on publication.

Agent source is `agents/<ui-name>/AGENT.md` plus sibling `SYSTEM.md`.
`AGENT.md` contains direct `agentlibre.agent/v1` metadata and required Tool
IDs; `SYSTEM.md` contains its instructions. Skills add bounded instructions,
required Tools and contained reference files, but cannot grant authority or
select implementations.
