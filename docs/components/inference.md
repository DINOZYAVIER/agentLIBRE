# Inference

Inference runs local models through llama.cpp profiles and records backend,
device, context, and response metadata.

First-party functions use tagged automatic runtime policies. At session start,
agentLIBRE matches the pinned package against embedded, measured CPU/GPU
profiles and resolves concrete numeric runtime values. A GPU load failure does
not silently change devices: interactive chat shows the eligible CPU plan and
asks, while non-interactive callers fail with a repair hint.

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

## Model-manager admission

The process-wide model manager owns one native worker and a bounded FIFO of
live pending commands. An active command is reported separately and does not
consume pending capacity. Cancelling or expiring queued generation removes its
exact queue entry immediately, so replacement work can be admitted without
waiting for the active native decode. Shutdown closes admission out of band,
cancels pending generation, fails pending management work as unavailable, and
drops contexts before their model.

The environment-configured AGL-139 native smoke exercises one reused model,
two independent contexts, queued and active cancellation, replacement work,
and native drop order:

```bash
AGL_LOCAL_INFERENCE_CONFIG=/private/resolved-inference.toml \
AGL_INFERENCE_ARTIFACT_ROOT=/private/inference-evidence \
AGL_STORE_ROOT=/private/store \
cargo test -p agl-inference manual_llama_cpp_smoke_from_env -- --ignored --nocapture

scripts/validate-agl139-smoke.py
```

The validator reads `.agl/smoke/AGL-139/native-manager.json`. That summary
contains only typed IDs, digests, counters, relative event references, and
pass/fail observations; prompts, generated text, model paths, private roots,
and native logs remain outside it.
