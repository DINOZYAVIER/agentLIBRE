# Events and progress

`agl-core` defines the bounded structural `AgentEventData` variants emitted
by `AgentFsm` and `AgentOperationFsm`. The daemon Store assigns one global monotonic
`AgentEventId` and timestamp while committing accepted state and messages in
the same SQLite transaction.

Durable events contain identities, lifecycle states, delivery attempts,
usage, and message references. They do not contain prompts, Tool input or
output, model content, error strings, stack traces, or engine diagnostics.
Observation consumers query this canonical Store journal.

`AgentProgress` is the separate transient stream owned by `agl-daemon`. It
contains model-output deltas and operation status only; it is never written to
SQLite or JSONL. A lagged subscriber rereads `AgentRunView` and durable events
from its cursor. Transient deltas are not replayed.
