# Matrix

`agl-matrix-bridge` forwards allowed Matrix text messages to the local Agent
daemon. A root message starts a Matrix thread and one `ConversationId`; replies
inside that thread continue the same conversation. It persists automatic
thread bindings and a bounded 4,096-entry crash/replay journal, then replies in
the originating thread. It supports stored sessions, password login, E2EE and
SAS self-verification.

Matrix is only a chat transport. It has no room commands, command prefix,
manual binding or explicit Tool invocation syntax. Any internal Tool call is
governed only by the admitted Agent snapshot.

Replies are sent directly from the inbound handler. There is no separate
notification outbox or Matrix-owned Store schema.

Each bridge process configures one absolute `agl.function_path` and one
`agl.workspace_path`. A new thread activates that Function/workspace and
durably freezes the resulting snapshot. Existing threads resolve their stored
Conversation after bridge or daemon restart and do not reread changed bridge
configuration or a relocked Function. The bridge has no Function switch or
manual send command.
