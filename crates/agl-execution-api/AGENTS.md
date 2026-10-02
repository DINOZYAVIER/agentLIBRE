# AGENTS.md

`src/protocol.rs` defines validated execution requests, owners, isolation,
terminal sizes, signals, outcomes and output/status frames. `src/client.rs`
implements async and blocking Unix-socket clients; `src/lib.rs` exports the
API. Child-process, launcher and SQLite implementation live in
`products/services/agl-execd/`.
