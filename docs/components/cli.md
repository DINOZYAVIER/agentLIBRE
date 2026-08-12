# CLI and interactive terminal UI

`agl` is the command-only agentLIBRE automation surface. Bare `agl` is a
read-only overview: it reports daemon compatibility and bounded session status,
names `agl-terminal` as the interactive application, and starts or mutates
nothing. Function-first automation remains explicit:

```bash
agl run --prompt "Summarize this repo."
agl serve
agl --function coding run --prompt "Inspect the tests."
```

Durable sessions have eight scriptable operations, all with deterministic text
and `--json` output:

```bash
agl session new
agl session list
agl session show SESSION_ID
agl session resume SESSION_ID
agl session submit SESSION_ID --prompt "Continue."
agl session follow SESSION_ID
agl session cancel SESSION_ID
agl session finish SESSION_ID
```

`follow` detaches on EOF or `Ctrl+C` without cancelling the session. `cancel`
stops active agent work but leaves the durable session open; `finish` is the
authoritative terminal state. Unknown or finished sessions are typed errors and
are never silently replaced.

`agl-terminal`, built and installed by the independent terminal repository,
owns interactive Chat and terminal presentation. It composes the bounded agent
client with the independent terminal client; canonical agent transcripts stay
in agentLIBRE and terminal execution/state stays in `agl-terminald`.

Within `agl-terminal`, Chat and persistent Unix terminal views remain peers:
a physical `!` enters the Shell composer, Enter on an empty Shell composer
attaches the Human terminal, and the trusted prompt/`Esc`+`!` gestures return to
Chat. Switching views does not stop shells, foreground programs, jobs, cwd, or
exports. `/disconnect` releases only the UI client; `/exit` explicitly finishes
the agent session. Human terminal bytes, command history, raw arguments,
secrets, and host paths never enter model context or safe activity projection.

Session projections are bounded and digest-verified before display. The UI's
history and reducer caches are disposable; neither can become a canonical
agent transcript or terminal lifecycle store. Host terminal creation remains
an explicit same-UID confirmation and is never an upgrade of workspace or
model authority.

`agl init` downloads and validates the selected pinned package, stages explicit
bindings, runs a normal model-manager smoke, and only then writes the workspace
default function in `.agl/workspace.toml`:

```toml
[functions]
default = "gemma4-e4b"
```

Model-backed execution always resolves a package-bound Function. The removed
raw inference namespace and local config override have no compatibility path.

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
