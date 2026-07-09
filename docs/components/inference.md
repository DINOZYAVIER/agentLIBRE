# Inference

Inference runs local models through llama.cpp profiles and records backend,
device, context, and response metadata.

Top-level runtime commands (`agl run`, `agl chat`, and `agl serve`) should reach
inference through a resolved agentFUNCTION. The function supplies the default
model config or profile, and CLI flags such as `--config` are per-invocation
overrides on that function.

Direct inference remains available for backend debugging, model smoke tests,
and config repair through an explicit low-level namespace:

```bash
agl inference run --config /path/to/local.toml --prompt "Reply once."
agl inference chat --config /path/to/local.toml
agl inference serve --config /path/to/local.toml
```

Low-level inference commands do not inject function context and should not emit
function resolution evidence.
