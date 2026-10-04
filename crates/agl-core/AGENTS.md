# AGENTS.md

## Module map (`src/`)

- `agent/`: Conversation/Run/Operation values, events/messages, context and inference snapshots, FSMs, compaction and memory codecs.
- `implementation_plan.rs`: validated plans, decisions, slices, workspace evidence and verification/result values.
- `package.rs`: package identities, versions, relative paths, digests and package-view interfaces.
- `content.rs`: validated text/image references, source/digest bindings and text size bound.
- `effect.rs`, `extension.rs`, `tool.rs`: grants, canonical JSON/schema and Extension/Tool definitions.
- `correlation_id.rs`, `ids.rs`: request/run/conversation/message IDs and logical identifiers.
- `fsm.rs`: shared FSM/transition interfaces; `lib.rs` exports the domain API.

`tests/agent_conformance.rs` checks Agent FSM behavior. Service scheduling,
SQLite persistence and process supervision live in products.
