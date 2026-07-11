# AGENTS.md

`agl-loop` orchestrates a turn through model requests, capability dispatch,
observations, and terminal events. Keep it backend-agnostic: external work is
represented by serializable `TurnEffect` values and enters again only as a
matching `TurnEffectResult`.
