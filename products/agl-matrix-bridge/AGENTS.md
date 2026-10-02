# AGENTS.md

## Module map (`src/`)

- `main.rs`, `runtime.rs`: CLI/runtime startup, Matrix login/sync, encryption and device verification.
- `config.rs`: bridge TOML and explicit session/store/binding paths.
- `access.rs`: room/user access decisions.
- `handler.rs`: inbound events and outbound actions.
- `app.rs`: bridge coordination against the `AgentBoundary` interface from `lib.rs`.
- `client.rs`: lazy daemon client integration.
- `state.rs`: thread/conversation bindings and replay reservations.
- `sync_retry.rs`: sync retry and permanent-failure classification.

`examples/config.toml` shows configuration; `tests/bridge_surface.rs` and
`src/runtime/tests.rs` cover bridge behavior. Agent execution belongs to the
daemon, accessed through `agl-daemon-api`.
