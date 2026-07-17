# Processes

Process execution is a first-party, capability-checked runtime service. It
supports exact argv programs, admitted shell commands with a real PTY, logical
working directories, background ownership, bounded private output, and daemon
operator controls.

## Selecting the tools

The core `process` skill from the `agl` pack routes these actions:

- `process.pwd`, `process.cd`;
- `process.exec`, `process.start`;
- `process.status`, `process.read`, `process.write`, `process.resize`,
  `process.kill`;
- `shell.exec`.

The skill is not selected by default. Select it explicitly with `--skill
process`, a function's `skills.use`, or the equivalent trusted runtime
selection. Function allow/deny policy, tool mode, provider trust, and dynamic
grants still apply after routing. Execution actions require `execute` tool
mode; selecting a skill never upgrades the mode by itself.

Use `process.exec` for a foreground argv and `process.start` for a background
argv. Spaces, quotes, `;`, `|`, redirects, globs, `$`, and `$()` are literal
argument data. Use `shell.exec` only when shell parsing is intentional. It
passes the command as one argument to the shell profile frozen at session/run
admission rather than consulting a later `$SHELL`, `PATH`, or symlink change.

## Workspace and host profiles

`workspace` is the default profile. On supported Linux hosts it creates user,
PID, mount, and network namespaces, mounts a private `/proc`, applies Landlock
and seccomp, disables network syscalls, drops capabilities, and exposes only:

- the workspace as writable;
- a per-execution home at `/.agl-private/home` and private `/tmp` as writable;
- standard runtime paths and configured `runtime_read_only_roots` as
  read-only.

The host-side state paths backing private home and temporary storage are never
mounted at their host names inside the target. This keeps disposable
workspaces below a host `/tmp` visible without exposing the supervisor's state
tree.

An executable outside the workspace, standard runtime roots, and configured
read-only roots fails with `sandbox_executable_unavailable`; it is not mounted
just because it appeared in `PATH`.

Configured runtime roots are also a supervisor-side authority ceiling. Every
root carried by an execution request must equal or be nested below one of
those canonical operator-configured directories. An internal caller may
narrow that view but cannot extend it before launch.

`host` is a separate conditional effect. It requires an exact active
`host_process_execution` grant for the action and scope. Login shell startup
requires both that effect and `shell_login_startup`. A missing, revoked,
expired, differently scoped, or stale-policy grant fails before target spawn.
Never switch to `host` merely because the workspace sandbox is unavailable.

## Working directory and shell state

`process.pwd` reports the logical durable cwd. `process.cd` changes it without
calling process-wide `chdir`; simultaneous sessions and child runs therefore
remain independent. An invocation-level `cwd` applies to only that execution.
`/workspace [PATH]` atomically resets the workspace and logical cwd, while a
child run receives a snapshot and cannot mutate its parent or siblings.

The admitted shell profile is configured under `[execution.shell]` with an
executable, non-login command argv, and optional login command argv. Workspace
shells are always non-login. Shell text is never reconstructed from an argv
request. Admission freezes the executable's canonical target and SHA-256
digest, both argument vectors, and the set of inheritable environment names.
Each launch rechecks the digest and executes a retained executable descriptor;
later config, `PATH`, symlink, file replacement, or newly inherited environment
names cannot substitute a different shell profile.

## Ownership, attach, and output

Each command receives a durable `ExecutionId` such as `exec_...`. Foreground
commands wait for a terminal state; background commands return the identity
immediately. Session-owned work ends with its session. Run-owned work follows
the run/grant lifetime. A child never inherits a parent execution identity,
cursor, or input lease.

Raw stdout, stderr, and PTY bytes are stored in private `0700` directories and
`0600` spool files. Reads are bounded and cursor-based, preserving invalid UTF-8
as base64 protocol chunks. Safe events, transcripts, and default status output
exclude argv, environment values, stdin, output previews, raw PIDs/file
descriptors, and spool paths. `--private-command` is an explicit same-user
operator request for a bounded argv display; it still omits environment
values.

The default retained-output ceiling is 64 MiB per execution. Reaching it starts
immediate tree termination, retains only the admitted termination headroom,
records discarded bytes, and marks output truncated. Finished output expires
after seven days by default. A later read returns `output_expired`, not an empty
success.

One writable attachment is allowed; any number of bounded read-only observers
may replay. CLI attach enters raw mode only after the server accepts it,
forwards resize events, and restores termios and signal handlers on every exit
path. The attach request's `RequestId` is the one lease/attachment identity at
every layer; no hidden terminal identity is minted. Press `Ctrl-]` to detach
without killing the target.

Attach delivery is bounded. If a client cannot keep up, the connection closes
with its last delivered sequence so it can resume without guessing a cursor.
Cancellation and deadline checks also cover launcher admission. Cancellation
kills the not-yet-admitted launcher/tree and commits one durable `cancelled`
result; an elapsed deadline commits `timed_out` instead of collapsing both
conditions into the setup timeout. Graceful termination that escalates to
`SIGKILL` records a safe `forced_termination` lifecycle event in the same
monotonic sequence space as output metadata.

## Operator commands

Daemon-owned executions are controlled through the private same-user daemon
socket:

```text
agl process list [--session SESSION_ID] [--run RUN_ID] [--all] [--json]
agl process status EXECUTION_ID [--private-command] [--json]
agl process read EXECUTION_ID [--after SEQUENCE] [--max-bytes N] [--json]
agl process attach EXECUTION_ID [--after SEQUENCE] [--read-only]
agl process kill EXECUTION_ID [--immediate] [--yes] [--json]
agl process doctor [--json]
```

Direct chat provides `/pwd`, `/cd PATH`, `/cd --host PATH`, `/processes`,
`/attach EXECUTION_ID [--read-only]`, and `/kill EXECUTION_ID [--immediate]`.
Those commands address that chat's in-process supervisor. A separate top-level
CLI does not claim process-local direct-chat handles.

`agl process doctor` reports launcher, namespace, Landlock, seccomp, pidfd, and
PTY preflight fields. Linux target construction fails closed if any required
primitive is unavailable. The public API compiles on other platforms and
returns `platform_unsupported` before creating execution state; native Windows,
macOS, and WSL behavior is not implemented or claimed.

## Crash semantics

Commands are at-most-once effects. The owner kills its process namespace during
normal shutdown, but a daemon crash cannot prove the command's final exit or
side effects. On restart, every prior-owner live execution and its linked step
becomes `outcome_unknown`; agentLIBRE retains safe metadata/output and never
automatically reruns the command.

A future graphical terminal UI will attach to the same protocol. It is not part
of the current process implementation.
