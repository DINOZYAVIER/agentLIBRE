# Inference

Inference runs local models through llama.cpp profiles and records backend,
device, context, and response metadata.

At session start, agentLIBRE matches the selected profile from the resolved
Model v3 package against static host capabilities and freezes its exact numeric
runtime values into an opaque plan. Live RAM/VRAM admission can reject that
plan but cannot select another profile or device.

Top-level runtime commands (`agl run`, bare `agl`, and `agl serve`) all reach
inference through a resolved Function and the same `InferenceHost`. Backend
smoke coverage invokes the private host/server contract directly; it is not a
second product runtime surface.

## Model-manager admission

The process-wide `InferenceHost` owns one constrained native server and a bounded FIFO of
live pending commands. An active command is reported separately and does not
consume pending capacity. Cancelling or expiring queued generation removes its
exact queue entry immediately, so replacement work can be admitted without
waiting for the active native decode. Shutdown closes admission out of band,
cancels pending generation, fails pending management work as unavailable, and
drops contexts before their model.

The AGL-173 native smoke exercises descriptor-backed load, bounded streaming,
one reused Model and explicit context/model release:

```bash
AGL_LLAMA_SERVER=target/llama-cpp/build/bin/llama-server \
AGL_TEST_MODEL_GGUF=/private/model.gguf \
cargo test -p agl-inference --test agl173_live_server -- --ignored --nocapture
```
