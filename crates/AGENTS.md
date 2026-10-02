# AGENTS.md

This directory contains the Rust workspace crates.
Keep crate boundaries explicit: shared APIs and schemas belong in their owning crate, and new cross-crate dependencies must be added deliberately in the workspace manifest.
Workspace crate and package names must use the `agl-*` prefix.

## Crate map

| Crate | Responsibility |
| --- | --- |
| `agl-core` | Validated domain values, Agent/Operation FSMs, messages, grants, memory and implementation-plan types. |
| `agl-daemon-api` | Public daemon commands/replies, Unix-socket client and progress/subscription frames. |
| `agl-execution-api` | Execution request/status/output protocol and async/blocking Unix-socket clients. |
| `agl-runtime` | Package manifests, Forge resolution, Function activation, model acquisition and inference. |
| `agl-tool-api` | Native Tool context, cancellation, handler and binding interfaces. |
| `agl-tools` | Filesystem, Forge, execution and SearxNG Tool implementations. |

Runnable products and service implementations are under `products/`, not here.
