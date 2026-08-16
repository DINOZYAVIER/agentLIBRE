# Terminal runtime

The terminal runtime is currently supported on Linux. Managed interactive
sessions support Bash and Zsh. Exact argv processes may run without a terminal
and do not require a terminal size.

## Install and verify

Preview an immutable local installation, then install it from a clean checkout:

```sh
scripts/terminal/install.sh --dry-run
scripts/terminal/install.sh
```

The default prefix is `~/.local`; pass `--prefix /absolute/path` to select a
different root. The installation publishes these commands:

- `agl-terminal` — interactive UI;
- `agl-terminald` — terminal service.

The process launcher remains a private sibling inside the selected generation.
It is deliberately not installed on `PATH` and is not a supported ambient
command.

Preview the verified systemd user units before applying them:

```sh
scripts/terminal/systemd-user-service.sh
scripts/terminal/systemd-user-service.sh --apply --enable
```

After installing a new generation, restart the service with:

```sh
scripts/terminal/systemd-user-service.sh --apply --restart
```

The unit installer verifies the generation directory, file inventory, modes,
hard-link counts and SHA-256 values before writing any unit. It refuses
unmanaged unit files and drop-ins.

## Runtime locations

With the default XDG locations, the service uses:

- socket: `$XDG_RUNTIME_DIR/agl-terminal/terminal.sock`;
- data: `$XDG_DATA_HOME/agl-terminal` or `~/.local/share/agl-terminal`;
- state: `$XDG_STATE_HOME/agl-terminal` or `~/.local/state/agl-terminal`;
- user configuration: `$XDG_CONFIG_HOME/agl-terminal` or
  `~/.config/agl-terminal`.

The socket and state directories are private to the current user. Installation
fails when a managed path traverses a symlink, has an unexpected owner or mode,
or can be modified by another user.

## Failure behavior

The UI and service verify that they belong to the same immutable generation.
A missing file, changed digest, incompatible protocol identity or broken
generation link stops startup before an operation is accepted.

Linux process execution also fails closed when the required namespace,
Landlock, seccomp, pidfd or PTY facilities are unavailable. Managed shell
startup accepts only the configured Bash or Zsh profile; there is no generic
`/bin/sh` fallback.

## Uninstall

Preview removal, stop the service, then remove only the selected binaries and
links:

```sh
scripts/terminal/uninstall.sh
systemctl --user stop agl-terminald.service agl-terminald.socket
scripts/terminal/uninstall.sh --apply
```

Uninstall retains terminal data and state. Remove those directories separately
only when their contents are no longer needed.
