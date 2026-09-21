# agentLIBRE Docs

This directory is the human-facing documentation map for agentLIBRE.

## Guides

- [Build](guides/build.md) - local build and install commands for development.
- [NixOS](guides/nixos.md) - Nix/NixOS wrapper flow for Vulkan and llama.cpp development.

## Components

- [CLI](components/cli.md) - command-line entrypoint and operator surface for `agl`.
- [Functions](components/functions.md) - declarative agent environments, exact locks, and visible source layout.
- [Composition](components/runtime.md) - the daemon-owned Store, inference, Agent, and Extension construction path.
- [Inference](components/inference.md) - shared local GGUF services, batching, and exact runtime realization.
- [Agents](components/agents.md) - durable AgentRun/AgentOperation state and the two pure FSMs.
- [Skills](components/skills.md) - thin instruction packages with Tool requirements and contained reference files.
- [Tools](components/tools.md) - neutral declarations, typed bindings, authority checks, and built-ins.
- [Execution](components/execution.md) - process and PTY ownership in `agl-execd`.
- [Store](components/store.md) - the single SQLite baseline for Runs, Operations, messages, events, and inference health.
- [Daemon](components/daemon.md) - long-running composition root for Agent execution.
- [Matrix](components/matrix.md) - inbound Matrix room/thread communication.
- [Events](components/events.md) - durable structural AgentEvent data and transient AgentProgress.

## Authoring specifications

Design and decision documents are maintained in the agentLIBRE authoring
repository under Ayeque Forge.
