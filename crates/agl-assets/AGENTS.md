# AGENTS.md

`agl-assets` owns builtin prompt and skill asset embedding. Keep this crate free
of CLI, inference, runtime path, and loop dependencies. Builtin assets must be
available from the compiled binary and expose deterministic hashes.
