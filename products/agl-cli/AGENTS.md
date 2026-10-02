# AGENTS.md

This crate owns the user-facing `agl` CLI entrypoint.
It owns argument parsing, configuration composition, the interactive REPL, and
human-readable rendering. Delegate Agent state to `agl-daemon` and every child
process or PTY to `agl-execd`.

## Directory map

- `Cargo.toml`: the `agl-cli` library and `agl` binary, with product dependencies.
- `src/main.rs`: calls `agl_cli::run_cli`; `src/lib.rs` dispatches commands and composes the daemon.
- `src/AGENTS.md`: map of configuration, artifact, Function Family, rendering and REPL modules.

The `agl serve` command hosts the daemon; there is no separate daemon binary.
