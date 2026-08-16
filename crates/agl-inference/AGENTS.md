# AGENTS.md

`agl-inference` owns the host-safe inference APIs, queue, admission ledger,
attempt journal, engine adapter, and process supervisor. Native llama.cpp,
ggml, and accelerator state belong only to the constrained subordinate
`llama-server` process; do not add an in-process production runtime or native
linkage here.

Failures should flow through typed `Result` values plus observation artifacts,
not panics or synthetic successful responses. An engine-generation failure
must reap that generation and release its reservations without terminating the
host.
