# agl-app boundary

`agl-app` owns presentation-neutral application commands, session projection,
prompt admission, terminal orchestration contracts, and private Human shell
history brokerage.

It must not depend on CLI, protocol, daemon, socket, HTTP, Ratatui, or
Crossterm types. Human-terminal commands and output must never be copied into
Chat projection or converted into model-turn input.
