# AGENTS.md

`agl-inference` owns the host-safe inference contracts, queue, admission ledger,
worker protocol, and worker supervisor. Native llama.cpp, ggml, and accelerator
state belong only to the sibling `agl-inference-worker` process; do not add an
in-process production runtime or native linkage here.

Failures should flow through typed `Result` values plus observation artifacts,
not panics or synthetic successful responses. A worker-generation failure must
reap that generation and release its reservations without terminating the host.
