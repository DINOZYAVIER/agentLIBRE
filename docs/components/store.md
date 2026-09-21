# Store

The private `agl-daemon::store` modules are the sole SQLite owner for Agent
Runs, Operations, messages, structural events, conversations, and inference
safety health. Storage is not a public workspace package.

A Conversation row owns one immutable exact Function reference, workspace
root, full `AgentRunSnapshot`, optional unique display name, and activity
timestamps. Resume and rename operate on this durable row; they never replace
its Function or workspace binding.

One private serialized writer performs all state-changing transactions and
global `AgentEventId` allocation. Up to four private read-only WAL connections
serve bounded observation queries outside the writer mutex. Callers receive a
cloneable typed `StoreHandle`; raw connections, repository traits, a public
pool, and a Store actor are not exposed.

The alpha cutover uses one current schema baseline. An incompatible managed
database and its WAL sidecars are deleted and recreated; there is no row
migration or legacy reader. Indexed identities, status, revisions, counters,
timestamps, roles, visibility, and digests use constrained scalar/BLOB
columns. Bounded canonical JSON stores composite values with unknown fields
rejected by their Rust types.

Accepted Assistant and Tool content exists once on `AgentMessage`; the owning
Operation stores a `MessageId` reference and reconstructs its typed logical
result during reads and recovery. Conversation queries enforce
`MessageVisibility::Conversation` before data reaches daemon or UI code.
Preserved private reasoning remains in the owning operation metadata and is
joined only while materializing model context; deleting the Conversation
cascades through its Runs and removes that reasoning.
