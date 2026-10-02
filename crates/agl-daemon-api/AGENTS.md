# AGENTS.md

`src/agent.rs` defines daemon commands, replies, errors, plan/activation views
and subscription frames. `src/client.rs` implements the async Unix-socket
client and subscriptions. `src/progress.rs` defines transient progress values;
`src/lib.rs` exports them. Daemon scheduling and Store implementation live in
`products/services/agl-daemon/`.
