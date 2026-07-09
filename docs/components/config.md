# Config

Config covers XDG paths, workspace settings, inference profiles, and runtime
options.

Use `agl config paths` for raw resolved paths and `agl config status` for a
health report that checks the runtime config, active local inference profile,
logs, session/store roots, and skill trust store.

The active local inference profile is resolved in this order:

1. `--config PATH` on `agl run`, `agl chat`, `agl serve`, or `agl config status`.
2. `AGL_LOCAL_INFERENCE_CONFIG`.
3. `local_inference_config` from `agl config paths`.

The runtime config is `runtime_config` from `agl config paths`. Create a starter
file with:

```bash
agl config init
```

Changing logging or workspace runtime config affects the next command
invocation. Changing the local inference profile or model requires starting a
new `agl run`, `agl chat`, or `agl serve` process.
