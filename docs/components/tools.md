# Tools

Tools are explicitly callable runtime operations guarded by trust, permissions, and state-effect boundaries.

Process and shell tools are routed by the unselected core `process` skill from
the `agl` pack.
They require execute mode, and host/login requests additionally require exact
conditional grants. See [Processes](processes.md) for the action set and
execution boundary.
