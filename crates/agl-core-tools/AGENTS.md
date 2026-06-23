# AGENTS.md

`agl-core-tools` contains compiled first-party tool implementations.
Keep filesystem and other concrete runtime behavior out of pure FSM crates.
Tools must validate their own safety boundary before touching the host.
