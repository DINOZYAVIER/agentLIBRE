# AGENTS.md

This directory contains CLI implementation code.
Keep it to service composition/configuration, `agl-daemon-api` commands, and
the terminal REPL. Model request/output codecs belong to
`agl-runtime::inference`; Agent FSM data belongs to `agl-core`, while admission
and scheduling belong to private `agl-daemon::agent` modules. Process and PTY
lifecycle belongs to `agl-execd`.
