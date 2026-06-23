# AGENTS.md

`agl-hooks` executes local script hooks through the shared hook contract.
Keep execution direct and explicit: no shell interpolation, no remote hooks,
and no trust bypasses for mutable scripts.
