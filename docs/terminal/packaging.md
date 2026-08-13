# Packaging and runtime identity

`scripts/terminal/install.sh` installs one immutable local generation containing the
public `agl-terminal` UI, `agl-terminald`, its private `agl-process-launcher`
sibling, and a manifest of their SHA-256 digests. The UI and service receive
public executable links. The launcher is never installed as an ambient
command.

The service environment must set:

- `AGL_TERMINALD_SOCKET`;
- `AGL_TERMINALD_LAUNCHER` to the installed private sibling;
- `AGL_TERMINALD_DATA_ROOT`;
- `AGL_TERMINALD_STATE_ROOT`; and
- `AGL_TERMINALD_BUILD_ID` to the manifest's service digest.

`scripts/terminal/systemd-user-service.sh` validates the installed service and private
launcher against the manifest, then renders the environment, socket, and
service as a read-only dry run. Pass `--apply`, optionally with `--enable` and
`--restart`, to publish them atomically. The socket lives below
`XDG_RUNTIME_DIR`; durable state and data use their separate XDG roots.
Package managers may render the supplied templates equivalently but must
preserve exact sibling, digest, descriptor-name, and activation checks.

`scripts/terminal/uninstall.sh` validates managed paths and defaults to a read-only
preview. `--apply` removes only the selected generation and links after the
service is stopped. It never removes terminal data or state automatically.
