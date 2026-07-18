# Config

Config covers XDG paths, workspace settings, function defaults, inference
profiles, and runtime options.

Use `agl config paths` for raw resolved paths and `agl config status` for a
health report that checks the runtime config, active local inference profile,
logs, session/store roots, and skill trust store.

Function-backed runtime commands resolve an agentFUNCTION before loading a
model. `agl init` writes the workspace default in `.agl/workspace.toml`:

```toml
[functions]
default = "gemma4-e4b"
```

`agl run`, bare `agl`, and `agl serve` use that function when `--function` is
omitted. `--config PATH` on the non-interactive runtime commands overrides the
selected function's model config for one invocation; it does not disable
function context, skills, tools, subagents, memory policy, identity hooks, or
function evidence. Interactive sessions use the daemon's resolved profile.

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

The active local inference profile is resolved for low-level inference
commands, config health checks, and function profile resolution in this order:

1. `--config PATH` on `agl inference run`, `agl inference serve`, or
   `agl config status`.
2. `AGL_LOCAL_INFERENCE_CONFIG`.
3. `local_inference_config` from `agl config paths`.

The runtime config is `runtime_config` from `agl config paths`. Create a starter
file with:

```bash
agl config init
```

Changing logging or workspace runtime config affects the next command
invocation. Changing the selected function, local inference profile, or model
requires starting a new `agl run`, interactive session, `agl serve`, or
`agl inference ...` process.

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
program = "/bin/sh"
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
