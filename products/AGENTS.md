# AGENTS.md

- `agl-cli/`: `agl` binary and library; also hosts `agl serve`.
- `services/agl-daemon/`: Agent service implementation library.
- `services/agl-execd/`: execution service and private launcher binaries.
- `agl-matrix-bridge/`: Matrix integration binary and library.

Each product's AGENTS.md describes its modules. Shared domain/protocol/runtime
crates are in `../crates/`.
