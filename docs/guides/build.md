# Build

For development builds, use `scripts/build-llama-cpp.sh` for llama.cpp and
build the command-only agent CLI and its private inference worker:

```sh
cargo build \
  -p agl-cli \
  -p agl-inference-worker \
  --bin agl \
  --bin agl-inference-worker
```

For local installation into `~/.cargo/bin`, use:

```sh
scripts/install-agl-cargo.sh
```

The installer resolves one explicit root from `--root`, `CARGO_INSTALL_ROOT`,
`CARGO_HOME`, or `~/.cargo` in that order. It stages the agent artifacts under
that root, publishes a complete immutable generation through one atomic
`current` pointer, and keeps only the stable public `bin/agl` symlink.
Already-running processes therefore keep using their original generation while
new invocations use the newly published generation. The installer serializes
publication with a private root lock and retains prior generations; it does not
garbage-collect them. The systemd installer finds `agl` on `PATH` by default,
normalizes aliases to that managed stable entrypoint, validates its private
inference worker, and orders the daemon after the separately installed
`agl-terminald.service`. It
rejects mutable build-tree binaries such as `target/release/agl`; run those in
the foreground for development instead of installing them as the durable user
service.

As an alpha policy, the installer does not migrate older flat or combined
runtime generations. Move or remove an obsolete public
`agl-process-launcher` and its combined generation explicitly before installing.
The terminal repository independently installs `agl-terminal`, `agl-terminald`,
and the daemon's private launcher sibling.
