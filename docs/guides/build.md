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
Re-running it replaces CLI, execd, private launcher, Matrix bridge and the whole
managed engine files. Read-only runtime generations and their dependency/GC-root
directories remain in place. Existing binaries are replaced by rename rather than
truncated in place.

Daemon and execd socket/service units are regenerated with the installed binary
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

Control installed services separately:

```sh
scripts/restart-services.sh --stop
scripts/restart-services.sh
```

Both actions include the daemon/execd sockets; Matrix is included only if its
unit is installed. `--stop` succeeds when the services become inactive.

Verify deployment ordering, failed builds, repeated replacement, optional
Matrix, custom paths and dry runs without touching real services:

```sh
python3 scripts/ci/test-release-deployment.py
```
