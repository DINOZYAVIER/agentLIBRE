# AGENTS.md

`agl-tools` owns tool IDs, provider/catalog contracts, first-party tool
implementations, and first-party guard hook implementations. Keep concrete
runtime behavior out of pure FSM crates. Tools must validate their own safety
boundary before touching the host.
