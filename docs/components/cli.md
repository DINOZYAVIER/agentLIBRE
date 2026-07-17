# CLI

The CLI is the command-line entrypoint and operator surface for `agl`.

Top-level runtime commands are function-first:

```bash
agl run --prompt "Summarize this repo."
agl chat
agl serve
agl run --function coding --prompt "Summarize this repo."
```

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
agl inference chat --config /path/to/local.toml
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

Attach requires a local terminal. Press `Ctrl-]` to detach while leaving the
target alive. Direct-chat `/processes`, `/attach`, and `/kill` address only that
chat's process-local supervisor; the top-level commands address the daemon.
See [Processes](processes.md) for policy, privacy, retention, and crash
semantics.
