# agl-app boundary

`agl-app` owns presentation-neutral application commands, session projection,
prompt admission, and explicit operator user-shell coordination.

It must not depend on CLI, protocol, daemon, socket, HTTP, Ratatui, or
Crossterm types. User-shell projection data must never be converted into model
turn input.
