# Security boundary

agentLIBRE or another caller decides policy. agl-terminal accepts only a
bounded terminal-owned request containing an opaque caller owner, immutable
authority fingerprint, exact allowed operations, canonical roots, exact
program/shell identity, and bounded resource limits.

`agl-terminald` is the sole live process and terminal owner. It does not infer
authority from process IDs, paths, executable names, agent IDs, or mode labels.
It owns execution/terminal transitions, input leases, output spooling, recovery,
retention, and terminal-private state. Callers receive bounded projections, not
raw private terminal history or secrets.

The private `agl-process-launcher` is resolved by an exact absolute path and
must share the build identity compiled into `agl-pty`. Linux admission uses the
existing namespace, Landlock, seccomp, pidfd, parent-death, exact argv, and PTY
contracts. Unsupported native prerequisites fail closed.

The service protocol uses an exact protocol version, crate version, build
digest, and runtime generation identity. Clients must name the expected full
identity; mismatches fail before an operation is admitted.

Bash and Zsh are the initial conforming managed shell profiles. Other shells
must implement the same explicit profile contract and pass the native suite;
there is no `/bin/sh` or best-effort integration fallback.
