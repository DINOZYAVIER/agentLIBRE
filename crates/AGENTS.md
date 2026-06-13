# AGENTS.md

This directory contains the Rust workspace crates.
Keep crate boundaries explicit: shared contracts belong in their owning crate, and new cross-crate dependencies must be added deliberately in the workspace manifest.
Workspace crate and package names must use the `agl-*` prefix. Product names
such as `agentLIBRE` are reserved for binaries, UI labels, and user-facing
surfaces, not crate/package names.
