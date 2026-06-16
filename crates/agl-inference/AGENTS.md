# AGENTS.md

`agl-inference` defines inference backends and the in-process llama.cpp runtime.
Failures should flow through `Result` plus observation artifacts, not panics or synthetic successful responses.
