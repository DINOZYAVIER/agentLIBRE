# CLI

The CLI is the command-line entrypoint and operator surface for `agl`.

Top-level runtime commands are function-first:

```bash
agl run --prompt "Summarize this repo."
agl
agl --resume
agl serve
agl --function coding
```

On Linux, bare `agl` opens the daemon-backed interactive surface. Chat and a
persistent Unix terminal are peer views of the same durable session:

- type a physical `!` and press Enter in an empty Chat composer to enter the
  last Human terminal (creating its managed Bash/Zsh PTY on first use);
- at a trusted empty shell prompt, `!` followed by Enter returns to Chat
  without sending either byte to the shell;
- while Vim, a REPL, or another foreground program is active, press `Esc` then
  `!` within 750 ms. `Esc` reaches the program immediately, so Vim is left in
  Normal mode while Chat is visible;
- `!ls`, `!!`, multiline input, and pasted `!` remain literal input in their
  current view.

Changing views does not stop the shell, jobs, cwd, exports, aliases, Vim, or a
waiting REPL. The daemon continuously drains the PTY and the CLI uses filtered
raw passthrough; it does not embed a terminal emulator or reconstruct a hidden
screen. `/disconnect` releases this client while the durable session and
terminal continue. `/exit` finishes the session and terminates its work.

Session presentation snapshots are transferred in bounded, verified chunks and
assembled by the client before they replace the visible Chat projection. `/new`
and `/resume` keep the source session visible and subscribed until the target
snapshot and command catalog have both loaded successfully; a failed target
load leaves the source view usable.

Shell navigation is native terminal behavior, so the Chat command catalog has
no `/cd` or `/pwd`. `/processes`, `/attach`, and `/kill` operate on typed
daemon-owned execution identities. Human terminal bytes and command history
are private and are never added to model context automatically.

`/processes` also contains two visible typed actions for a separate Human
`HOST` terminal. **Open HOST terminal** uses managed startup and is the default
and recommended Host startup choice. **Open HOST terminal + user rc**
explicitly requests that the daemon resolve and source the normal Bash/Zsh rc
after managed setup. Both
actions show a Host-authority confirmation before sending any request; cancel
leaves the Workspace terminal unchanged. The CLI never accepts or sends an rc
path. An existing matching Host terminal is selected idempotently and attached
writable, with a visible `HOST` marker. These are picker actions, not slash
commands and not an authority upgrade of the Workspace terminal. Dedicated
`Ctrl+H`/`Ctrl+Shift+H` bindings are intentionally avoided because common
terminals cannot distinguish them reliably from Backspace/control encodings.

`agl init` downloads and validates the selected pinned package, stages explicit
bindings, runs a normal model-manager smoke, and only then writes the workspace
default function in `.agl/workspace.toml`:

```toml
[functions]
default = "gemma4-e4b"
```

Direct model/config execution is reserved for explicit low-level inference
commands:

```bash
agl inference run --config /path/to/local.toml --prompt "Reply once."
agl inference serve --config /path/to/local.toml
```

Inspect and control daemon-owned background processes with `agl process`:

```bash
agl process list --all
agl process status exec_...
agl process read exec_... --after 0
agl process attach exec_...
agl process kill exec_... --yes
agl process doctor --json
```

Standalone `agl process attach` requires a local terminal. Press `Ctrl-]` to
detach while leaving the target alive. The peer interactive Terminal view does
not reserve `Ctrl-]`; it uses the `!` view-switch sequences described above.
Interactive `/processes`, `/attach`, and `/kill` and the top-level process
commands address the same daemon-owned executions.
See [Processes](processes.md) for policy, privacy, retention, and crash
semantics.
