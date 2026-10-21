# agentLIBRE

agentLIBRE -- free and open-source agentic system.

A simple, concise coding agent for local models. It uses finite-state
machines to make local models more deterministic, without heavy
guard rails: the tools it calls keep their own protections.

The command is `agl` (also installed as `agentLIBRE`). Work in
progress: `agl -p "..."` runs one turn against an already running
llama-server; `agl` alone opens the interactive session (a line editor, a dock with the
state and the queue, streamed thinking and text, tool cards);
`agl server start` runs the project's llama-server as a systemd user unit
from `[server]` in the project's `agentLIBRE.toml` (the user's file holds
the defaults). Install with `cargo install
--path .`.
