# AGENTS.md

`agl-runtime` owns process runtime paths, runtime config, logging config, and
tracing initialization. Keep it independent from model execution, turn policy,
and chat session persistence.
