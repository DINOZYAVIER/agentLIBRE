# AGENTS.md

This directory contains the Rust workspace crates.
Keep crate boundaries explicit: shared contracts belong in their owning crate, and new cross-crate dependencies must be added deliberately in the workspace manifest.
