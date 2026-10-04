# AGENTS.md

This directory contains CLI implementation code.
Keep it to service composition/configuration, `agl-daemon-api` commands, and
the terminal REPL. Model request/output codecs belong to
`agl-runtime::inference`; Agent FSM data belongs to `agl-core`, while admission
and scheduling belong to private `agl-daemon::agent` modules. Process and PTY
lifecycle belongs to systemd through the daemon adapter.

## Module map

| Module | Responsibility |
| --- | --- |
| `main.rs`, `lib.rs` | Binary entrypoint, command dispatch, daemon composition, terminal output and foreground Run handling. |
| `cli.rs` | Clap arguments and command groups. |
| `attachments.rs` | Explicit human image selection, source/digest references and queued chat attachments. |
| `config.rs` | Human TOML settings, Function locators, config plans, generated active config and default-Function edits. |
| `workspace.rs` | XDG/AGL_HOME roots, active Function lookup, config apply/reload and doctor output. |
| `forge.rs` | List/add/update/remove/sync Forge declarations through the AGL workspace manifest. |
| `function_family.rs` | Family list/show/select, detected RAM/VRAM checks and explicit model-download confirmation. |
| `repl.rs` | Chat initialization and input/output loop; `repl/editor.rs` handles terminal input and `repl/session.rs` tracks conversations and subscriptions. |
| `render/mod.rs` | Shared terminal rendering helpers. |
| `render/content.rs`, `render/style.rs` | Content formatting and terminal styles. |
| `render/events.rs`, `render/commands.rs` | Event/progress presentation and REPL command presentation. |
