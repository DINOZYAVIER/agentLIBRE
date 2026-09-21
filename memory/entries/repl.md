# REPL architecture

- slug: repl
- created: run_01a090d4-5628-77d3-8f53-1abe34dbda5d
- updated: run_01a09292-e4ab-7e11-a6ee-1221312293c1
- artifact: reports/repl-architecture.md

## Claims

- The REPL is the default (no-subcommand) mode of the `agl` CLI. [src: msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9, msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513] [file: products/agl-cli/src/lib.rs@778b0789cbd7]
- Input uses reedline (Emacs keybindings) with SlashCompleter, ListMenu (Tab or /), DockHinter. [src: msg_01a090d5-e0fe-76e2-ab23-107cd6e69dd0] [file: products/agl-cli/src/lib.rs@778b0789cbd7]
- Daemon protocol: Unix socket, JSONL, schema `agentlibre.agent.v1alpha`, 8 MiB max frame. [src: msg_01a090d8-9e45-7923-8e85-0a14c31a29ec, msg_01a090d8-b3e5-75a1-89d9-26cc3ac272d4] [file: crates/agl-daemon-api/src/agent.rs@b9e79fa893fe, crates/agl-daemon-api/src/lib.rs@8ea676989617]
- Model generation is measured before the call; prompt ≥ 70% of capacity (ceil(7/10·cap)) fails with CompactionRequired. [file: crates/agl-core/src/agent/context.rs@3a4aa91013fd, products/services/agl-daemon/src/agent/operation_driver.rs@87d4b4ce57ad]
- Session lifecycle: the CLI connects to the daemon, opens (or resumes) a conversation; each submitted prompt starts a run via `AgentClient::start_run`; the REPL renders the run's `AgentEvent` stream live and prints a `RUN` summary with elapsed time on terminal status. [file: reports/repl-architecture.md@332f51b1a48c, crates/agl-daemon-api/src/client.rs@ef5d43ad6051]
- `AgentProgress` (`ModelOutputDelta`, `OperationStatus`) is process-local and never persisted; durable consumers resync via `AgentRunView` and `AgentEventPage`. [file: reports/repl-daemon-protocol.md@5d13c5f1ba2d, crates/agl-daemon-api/src/progress.rs@d2013283ecc0]
- Operation requests are typed JSON objects on the wire; `AgentOperationKind` has exactly three variants: `ModelGeneration`, `Compaction`, `Tool`. [file: reports/repl-daemon-protocol.md@5d13c5f1ba2d, crates/agl-core/src/agent/operation.rs@38b1858893c7]
- `AgentClient` (crate `agl-daemon-api`) exposes `open_conversation`, `start_run`, `subscribe`, `rename_conversation`. [file: reports/repl-daemon-protocol.md@5d13c5f1ba2d, crates/agl-daemon-api/src/client.rs@ef5d43ad6051]
- Prompt history is persisted as JSON lines to `<state>/repl/history.reedline` (XDG state root `agentLIBRE`), capped at 1000 entries with the oldest dropped. [file: reports/repl-terminal-ui.md@0dfba85063f6, products/agl-cli/src/lib.rs@778b0789cbd7]
- `TtyRenderer` owns the terminal display state for the session (theme, activity string, streaming buffer, transcript newline state); model markdown is rendered with ANSI colors. [file: reports/repl-terminal-ui.md@0dfba85063f6, products/agl-cli/src/lib.rs@778b0789cbd7]
