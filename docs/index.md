# agentLIBRE Docs

This directory is the human-facing documentation map for agentLIBRE.

## Guides

- [Quickstart](guides/quickstart.md) - minimal path from checkout to a working `agl`.
- [Build](guides/build.md) - local build and install commands for development.
- [NixOS](guides/nixos.md) - Nix/NixOS wrapper flow for Vulkan and llama.cpp development.
- [Local Inference](guides/local-inference.md) - GGUF profile setup and local model execution.

## Components

- [CLI](components/cli.md) - command-line entrypoint and operator surface for `agl`.
- [Runtime](components/runtime.md) - orchestration layer that connects config, skills, tools, inference, and evidence.
- [Inference](components/inference.md) - local model execution through llama.cpp profiles and runtime metadata.
- [Skills](components/skills.md) - git-pinned trusted instruction packs that shape context and tool routing.
- [Tools](components/tools.md) - explicitly callable runtime operations with permission and trust boundaries.
- [Artifacts](components/artifacts.md) - workspace data under `.agl`, including locks, tasks, reviews, and generated state.
- [Store](components/store.md) - SQLite-backed durable domain state, migrations, and idempotency records.
- [Daemon](components/daemon.md) - long-running local service for turn execution and background work.
- [Matrix](components/matrix.md) - Matrix bridge and outbox integration for room-scoped communication.
- [Memory](components/memory.md) - explicit user memory, suggestions, approval, and retrieval.
- [Notes](components/notes.md) - local notes database and promotion path into memory.
- [Cron](components/cron.md) - scheduled jobs for builtins and trusted skills.
- [Config](components/config.md) - XDG paths, workspace settings, inference profiles, and runtime options.
- [Events](components/events.md) - structured runtime and FSM evidence emitted during commands and turns.
- [Hooks](components/hooks.md) - validation and guard checks such as `repo_path.validate`.
- [Tasks](components/tasks.md) - implementation specs and decisions distributed through the tasks component.
- [Docs](components/docs.md) - documentation and future wiki-style synthesis over code, specs, and evidence.
