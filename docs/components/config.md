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
default = "gemma4-12b"
```

`agl run`, `agl chat`, and `agl serve` use that function when `--function` is
omitted. On those commands, `--config PATH` overrides the selected function's
model config for one invocation; it does not disable function context, skills,
tools, subagents, memory policy, identity hooks, or function evidence.

Packaged functions contain portable model ids. Bind those ids to files on this
machine in `$AGL_HOME/config/models.toml`:

```toml
version = 1

[models.gemma4-12b]
path = "/home/user/models/gemma4-12b.gguf"

[models.gemma4-12b-mmproj]
path = "/home/user/models/mmproj-gemma4-12b.gguf"
```

Bindings are explicit: agentLIBRE does not search home directories or infer a
model from its filename. `agl function status <id>` reports required ids,
resolved paths, and the binding file to repair when an id is missing.

The active local inference profile is resolved for low-level inference
commands, config health checks, and function profile resolution in this order:

1. `--config PATH` on `agl inference run`, `agl inference chat`,
   `agl inference serve`, or `agl config status`.
2. `AGL_LOCAL_INFERENCE_CONFIG`.
3. `local_inference_config` from `agl config paths`.

The runtime config is `runtime_config` from `agl config paths`. Create a starter
file with:

```bash
agl config init
```

Changing logging or workspace runtime config affects the next command
invocation. Changing the selected function, local inference profile, or model
requires starting a new `agl run`, `agl chat`, `agl serve`, or
`agl inference ...` process.
