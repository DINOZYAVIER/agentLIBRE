# AGENTS.md

`agl-execd` is the only production package allowed to create or control child
processes and PTYs. Keep public wire values in `agl-execution-api`; Linux file
descriptors, process IDs, launcher messages, and SQLite details remain private
here.

Every launched process must stay owned by the execd/launcher lifetime boundary.
Client disconnect is not cancellation. Execd death must terminate descendants,
and recovery must record an unknown outcome rather than inventing success.
