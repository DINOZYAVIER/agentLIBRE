# agl-matrix-bridge

`agl-matrix-bridge` connects Matrix rooms to an `agl serve` daemon over the
local `agl-client` protocol. Matrix SDK, sync, login, and verification stay in
this bridge crate; turn execution, sessions, tools, and model runtime stay in
the daemon.

This is an alpha bridge. It does not accept the old legacy `dyno` config shape.
Use the `!agl` command prefix unless `matrix.command_prefix` is changed.

## Setup

Build the bridge and start an agent daemon:

```sh
cargo build --release -p agl-cli -p agl-matrix-bridge
./target/release/agl serve --config /path/to/local-inference.toml
```

For a user-systemd daemon:

```sh
scripts/agentlibre-daemon-systemd-service.sh --dry-run
scripts/agentlibre-daemon-systemd-service.sh --enable --restart
./target/release/agl status
```

Copy `examples/config.toml` to
`~/.config/agentLIBRE/matrix-bridge/config.toml` and edit:

- `matrix.homeserver_url`
- `matrix.user_id`
- `matrix.session_path`
- `matrix.store_path`
- `agl.socket_path`
- `access.allowed_users`
- `access.allowed_rooms`
- `bindings.path`

Use absolute paths for service-managed runs.

Log in and save the Matrix session:

```sh
export AGL_MATRIX_USERNAME='agl-bot'
export AGL_MATRIX_PASSWORD='...'
./target/release/agl-matrix-bridge login-password \
  --config ~/.config/agentLIBRE/matrix-bridge/config.toml
```

For encrypted rooms, verify the bridge device from a trusted Matrix device:

```sh
./target/release/agl-matrix-bridge verify-device \
  --config ~/.config/agentLIBRE/matrix-bridge/config.toml \
  --user-id '@alice:example.org' \
  --device-id DEVICEID
```

Validate local config and daemon connectivity:

```sh
./target/release/agl-matrix-bridge check-config \
  --config ~/.config/agentLIBRE/matrix-bridge/config.toml
./target/release/agl-matrix-bridge status \
  --config ~/.config/agentLIBRE/matrix-bridge/config.toml
```

Install the user systemd service:

```sh
scripts/agentlibre-matrix-bridge-systemd-service.sh --dry-run
scripts/agentlibre-matrix-bridge-systemd-service.sh --enable --restart
```

## Room Smoke

In an allowed Matrix room:

```text
!agl status
!agl send Reply exactly: matrix bridge ok
```

If `matrix.normal_chat = true`, normal text messages from allowed users are also
sent to the daemon. Otherwise only `!agl send ...` sends a turn.
