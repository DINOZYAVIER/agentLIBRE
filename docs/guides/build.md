# Build

Build and deploy the complete locked Rust workspace and private Vulkan
`llama-server`, replace the user-systemd units and restart the services:

```sh
scripts/build-release.sh
```

The script enters the Nix Vulkan environment automatically. It resolves the
installation prefix from `CARGO_INSTALL_ROOT`, `CARGO_HOME`, or `~/.cargo`;
`--root /absolute/path` overrides it. It builds before stopping services and
stages all binaries/libraries, checking the relocated engine inventory first.
Re-running it replaces CLI, Matrix bridge and the whole
managed engine files. Read-only runtime generations and their dependency/GC-root
directories remain in place. Existing binaries are replaced by rename rather than
truncated in place.

Daemon socket/service units are regenerated with the installed binary
paths. Matrix is configured when its config file exists at
`~/.config/agentLIBRE/matrix-bridge/agl.toml` (or `AGL_MATRIX_BRIDGE_CONFIG`).
Services restart after a unit reload and failed/start-limit reset.

Preview commands without building or changing services, or install binaries
without changing units/services:

```sh
scripts/build-release.sh --dry-run
scripts/build-release.sh --no-services
```

For a debug Rust install, use `scripts/install-agl-cargo.sh --debug`;
add `--services` to update/restart units. The installer can be invoked from any
directory. It rebuilds llama.cpp even when a binary exists;
`--skip-llama-build` explicitly reuses an existing engine build.

On first installation, the installer creates `${data}/source/agentLIBRE.toml`
from `assets/default-catalog.toml` and runs `agl forge sync`. It installs
declarations and source files without downloading model weights. Missing
application settings are created with CPU Small as the default Function.
Existing settings and complete catalogs are preserved. If only the manifest
or lock exists, installation stops: run `agl forge sync` explicitly in that
catalog before retrying.

Control installed services separately:

```sh
scripts/restart-services.sh --stop
scripts/restart-services.sh
```

Both actions include the daemon socket; Matrix is included only if its
unit is installed. `--stop` succeeds when the services become inactive.

Verify deployment ordering, failed builds, repeated replacement, optional
Matrix, custom paths and dry runs without touching real services:

```sh
python3 scripts/ci/test-release-deployment.py
```

Execution requires tmux and systemd-run/systemctl on the daemon PATH, with a
working systemd user manager. Planner execution additionally requires
bubblewrap with `--bind-fd` (introduced in
[0.10.0](https://github.com/containers/bubblewrap/releases/tag/v0.10.0)) and
Nix store executables; the development Nix shell provides the
clients and GNU tools. A shell `agl serve` also creates a managed user service.

The current alpha Store baseline is 22. Generated active config includes
service_git and forge_sync_timeout_seconds to generated active config. Re-run config apply for the new format.
Older Store schemas are rejected; use the existing reviewed config/Store
rotation workflow rather than deleting data. Retire previously installed execd
units/binaries separately; the new installer does not manage that removed
product. Build into an isolated `--root /tmp/... --no-services` prefix when
checking a release without replacing the installed application.
