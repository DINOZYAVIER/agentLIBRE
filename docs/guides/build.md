# Build

For development builds, use `scripts/build-llama-cpp.sh` for llama.cpp and
build the CLI together with its required, separately packaged sibling process
launcher:

```sh
cargo build \
  -p agl-cli \
  -p agl-process-launcher \
  --bin agl \
  --bin agl-process-launcher
```

For local installation into `~/.cargo/bin`, use:

```sh
scripts/install-agl-cargo.sh
```

The installer resolves one explicit root from `--root`, `CARGO_INSTALL_ROOT`,
`CARGO_HOME`, or `~/.cargo` in that order. It stages both binaries under that
root, publishes a complete immutable generation through one atomic `current`
pointer, and keeps stable `bin/agl` and `bin/agl-process-launcher` symlinks.
Already-running processes therefore keep using their original generation while
new invocations use the newly published pair. The installer serializes
publication with a private root lock and retains prior generations; it does not
garbage-collect them. The systemd installer finds `agl` on `PATH` by default,
normalizes aliases to that managed stable entrypoint, and validates the
matching `agl-process-launcher` beside the resolved generation binary. It
rejects mutable build-tree binaries such as `target/release/agl`; run those in
the foreground for development instead of installing them as the durable user
service.

As an alpha policy, the installer does not migrate older flat regular binaries.
If either public command is a regular file instead of the managed symlink, move
or remove both commands explicitly before installing. On a fresh managed
install, both stable links are created while still non-runnable and one atomic
`current` publication makes the complete pair runnable together.
