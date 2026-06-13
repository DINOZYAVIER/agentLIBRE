# AGENTS.md

`agl-loop` orchestrates a turn through model requests, tool dispatch, observations, and terminal events.
Keep it backend-agnostic: external work enters through `AgentLoopHost`.
