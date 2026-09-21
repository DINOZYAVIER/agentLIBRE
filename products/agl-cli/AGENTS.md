# AGENTS.md

This crate owns the user-facing `agl` CLI entrypoint.
It owns argument parsing, configuration composition, the interactive REPL, and
human-readable rendering. Delegate Agent state to `agl-daemon` and every child
process or PTY to `agl-execd`.
