# AGENTS.md

This directory contains repository maintenance and runtime build scripts.
Keep scripts explicit about required tools and environment variables; avoid hidden dependencies on local machine state.

## Script map

| Scripts | Responsibility |
| --- | --- |
| `build-release.sh` | Locked workspace release, native engine build, deployment to the Cargo prefix and service refresh; `--no-services` installs binaries only. |
| `install-agl-cargo.sh`, `uninstall-agl-cargo.sh` | Install/remove CLI, Matrix bridge and private engine bundle. |
| `build-llama-cpp.sh`, `agl-nix-vulkan.sh` | Dynamic CPU/Vulkan engine build and Nix build environment wrapper. |
| `restart-services.sh` | Explicit user-systemd stop/restart, including socket units and Matrix bridge. |
| `agentlibre-*-systemd-service.sh`, `systemd-lib.sh` | Generate/install user units and configure service paths/environments. |
| `agentlibre-matrix-login-password.sh` | Matrix login helper. |
| `agl-daemon-live-smoke.sh`, `smoke-agentlibre-llama-cpp.sh` | Live model/daemon Agent-flow checks using an explicitly supplied GGUF. |
| `planning-implementation-live-e2e.sh` | Live planner approval, implementation and verification checks. |
| `bump-workspace-version.sh` | Checkpoint version preview/update, version commit and tag workflow. |
| `ci/` | CI tool installation/preparation, workspace checks, behavior-test checks and release smoke. |

`ci/test-release-deployment.py` checks installation/replacement and systemd
ordering with isolated binaries and a fake user systemd.
