# AGENTS.md

## Module map (`src/`)

- `lib.rs`: provider exports.
- `filesystem.rs`: contained file reads, writes and patch handling for `agentlibre.files`.
- `forge.rs`: declared-artifact reads for `agentlibre.forge`.
- `execution.rs`: command/terminal Tools through the execution client; planner commands request read-only isolation.
- `searxng.rs`: search provider, query validation and TLS credential configuration.

Handlers use `agl-tool-api` context/bindings. Tool/Effect schemas are in
`extensions/`; provider registration and Run admission are in the daemon.
