# AGENTS.md

`agl-session` owns chat session lifecycle state, transcript events, replay, and
session persistence.
Keep it independent from CLI presentation, runtime path discovery, tracing, and
model backend behavior.
