# AGENTS.md

`agl-process` owns portable process, PTY, I/O, lifecycle, and persistence-neutral
execution contracts. Keep model tool routing, chat/session persistence, daemon
protocol, CLI presentation, and runtime path discovery in their owning crates.

Public contracts must not expose raw process IDs, file descriptors, or private
spool paths as authority. Linux implementation details belong below
`platform::linux`; unsupported platforms must fail before attempting a spawn.
