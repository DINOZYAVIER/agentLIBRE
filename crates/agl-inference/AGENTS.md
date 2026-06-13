# AGENTS.md

`agl-inference` defines inference backends and the current llama.cpp CLI backend.
Failures should flow through `Result` plus observation artifacts, not panics or synthetic successful responses.
