# Build

For a release deployment, build and install all binaries plus the constrained
private `llama-server`:

```sh
scripts/build-release.sh
scripts/restart-services.sh
```

The release script resolves the installation prefix from `CARGO_INSTALL_ROOT`,
`CARGO_HOME`, or `~/.cargo`. Re-running it replaces the deployed binaries and
private engine libraries. It does not restart services until the explicit
restart command is run.

Install or preview the user-systemd socket/service separately:

```sh
scripts/agentlibre-execd-systemd-service.sh --enable
scripts/agentlibre-daemon-systemd-service.sh --enable
scripts/agentlibre-execd-systemd-service.sh --dry-run
scripts/agentlibre-daemon-systemd-service.sh --dry-run
```
