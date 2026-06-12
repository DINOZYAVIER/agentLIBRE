# Local llama.cpp Smoke

This manual smoke exercises the real `agl-inference` llama.cpp CLI backend
without requiring model or binary paths in normal unit tests.

Create a local config file:

```toml
[backend]
kind = "llama_cpp"
binary = "/path/to/llama-cli"
model = "/path/to/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
```

Run the ignored smoke with an explicit artifact root:

```sh
AGL_LOCAL_INFERENCE_CONFIG=/path/to/local-inference.toml \
AGL_INFERENCE_ARTIFACT_ROOT=/tmp/agentlibre-inference-smoke \
cargo test -p agl-inference manual_llama_cpp_smoke_from_env -- --ignored --nocapture
```

Expected evidence layout:

```text
<artifact-root>/inference-runs/manual-smoke/
  events.jsonl
  attempts/attempt-001/
    request.json
    response.json
    stderr.log
```
