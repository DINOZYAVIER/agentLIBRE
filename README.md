# agl-lite

A simple, concise coding agent for local models. It uses finite-state
machines to make local models more deterministic, without heavy
guard rails: the tools it calls keep their own protections.

Work in progress: `agl-lite -p "..."` runs one turn against an already
running llama-server; the interactive mode is not implemented yet. Build
with `cargo build --release`.
