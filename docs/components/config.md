# Config

Config covers XDG paths, workspace settings, function defaults, and host
runtime options. Model inference profiles belong to Model packages.

Use `agl config paths` for raw resolved paths and `agl config status` for a
health report that checks the runtime config, logs, session/store roots, and
skill trust store.

Function-backed runtime commands resolve an agentFUNCTION before loading a
model. `agl init` writes the workspace default in `.agl/workspace.toml`:

```toml
[functions]
default = "gemma4-e4b"
```

`agl run`, bare `agl`, and `agl serve` use that function when `--function` is
omitted. Every surface resolves the same Function-owned generation policy and
Model-owned runtime profile; there is no per-invocation fixed-config override.

Packaged functions contain portable model ids. Bind those ids to files on this
machine in `$AGL_HOME/config/models.toml`:

```toml
version = 1

[models.gemma4-e4b]
path = "/home/user/.cache/huggingface/hub/models--.../snapshots/COMMIT/model.gguf"

[models.gemma4-e4b-mmproj]
path = "/home/user/.cache/huggingface/hub/models--.../snapshots/COMMIT/mmproj.gguf"
```

Bindings are explicit: agentLIBRE does not search home directories or infer a
model from its filename. `agl init` normally creates these bindings from
validated HF cache entries. `agl function status <id>` reports required ids,
resolved paths, and the binding file to repair when an id is missing.

The runtime config is `runtime_config` from `agl config paths`. Create a starter
file with:

```bash
agl config init
```

Changing logging or workspace runtime config affects the next command
invocation. Changing the selected Function or Model package/profile requires
starting a new `agl run`, interactive session, or `agl serve` process.

## Inference residency

`[inference.residency]` sets one global bounded policy for idle native
resources. A reusable context is eligible for release after
`context_idle_seconds`; once the final context release is acknowledged, its
model is eligible after `model_idle_seconds`. Both values are integer seconds
in `1..=86400`.

```toml
[inference.residency]
context_idle_seconds = 900
model_idle_seconds = 300
```

## Process execution

`[execution]` controls bounded process supervision. Defaults include eight
active executions, a 120-second foreground timeout, a 30-minute maximum, a
64-KiB input/result message bound, a 64-MiB private spool, 1 MiB of termination
output headroom, and seven-day finished-output retention.

```toml
[execution]
max_active = 8
default_foreground_timeout_ms = 120000
maximum_foreground_timeout_ms = 1800000
termination_grace_ms = 2000
max_input_bytes = 65536
max_result_bytes = 65536
max_spool_bytes = 67108864
termination_output_headroom_bytes = 1048576
finished_retention_seconds = 604800
runtime_read_only_roots = ["/opt/project-runtime"]

[execution.shell]
program = "bash"
command_args = ["-c"]
login_command_args = ["-l", "-c"]

[execution.environment]
inherit = ["PATH", "LANG", "LC_*", "TERM", "COLORTERM", "TZ"]
maximum_bytes = 65536
```

Runtime roots must be existing canonical directories. They form the maximum
read-only view the process supervisor will admit; an execution request cannot
add another host path. The shell snapshot freezes its canonical executable
target and digest, exact argument vectors, and the matched environment-name
allowlist. Resumed sessions therefore ignore names added to a later runtime
config, while current values for already admitted names remain private and are
never emitted in normal status/events. See [Processes](processes.md).

For one-shot executions, `max_input_bytes` is a lifetime ceiling. For a
persistent managed terminal it is both the maximum size of one write and the
maximum pending input queue; successfully drained writes do not consume a
terminal-lifetime budget. The private spool ceiling remains cumulative, so a
terminal that produces unbounded output is still terminated safely.

Persistent workspace shells inherit only the configured admitted environment
names plus an explicit creation-time structured overlay. Values such as
`CLAUDE_*` may be supplied as ordinary terminal-private variables when their
names and sizes pass validation. Shell `export`/`unset` then affects only that
PTY and its children; it is never copied back into Chat, another terminal, or
an agent. Reserved `AGL_SHELL_INTEGRATION_*`/`AGL_TERMINAL_*` names and shell
hook variables are rejected.

Secret references are distinct from ordinary overlay values. The default
runtime has no secret backend and rejects them. An admitted resolver can hand a
resolved value to the Linux launcher through a sealed anonymous descriptor
after durable execution admission; the value is not added to the protocol DTO,
`ExecutionRequest`, execution repository, terminal fingerprint, logs, or debug
formatting. Only the explicitly authorized child environment receives it.
