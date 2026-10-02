# AGENTS.md

`agl-execd` is the only production package allowed to create or control child
processes and PTYs. Keep public wire values in `agl-execution-api`; Linux file
descriptors, process IDs, launcher messages, and SQLite details remain private
here.

Every launched process must stay owned by the execd/launcher lifetime boundary.
Client disconnect is not cancellation. Execd death must terminate descendants,
and recovery must record an unknown outcome rather than inventing success.

## Module map (`src/`)

- `main.rs`: standalone service entrypoint, XDG/AGL_HOME paths and launcher lookup.
- `lib.rs`: execution service configuration and public implementation surface.
- `server.rs`: Unix-socket server and execution command dispatch.
- `supervisor.rs`: child/PTY supervision, output collection, deadlines and termination.
- `sandbox.rs`: launcher boundary and Linux process isolation.
- `readonly.rs`: bubblewrap planner workspace view and per-Execution scratch creation/recovery.
- `store.rs`: SQLite execution records and recovery outcomes.
- `bin/agl-execd-launcher.rs`: private launcher binary.

`tests/service.rs` checks protocol/service behavior, supervision and isolation.
Planner isolation requires `bwrap` in the service environment.
