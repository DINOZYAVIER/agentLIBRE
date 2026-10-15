# agl-lite

A simple, concise coding agent for local models. It uses finite-state
machines to make local models more deterministic, without heavy
guard rails: the tools it calls keep their own protections.

The command is `agl`. Work in progress: `agl -p "..."` runs one turn
against an already running llama-server; `agl` alone opens the
interactive session (a line editor, a dock with the state and the queue,
streamed thinking and text, tool cards). Install with
`cargo install --path .`.
