# Local Inference

Local inference uses GGUF files through llama.cpp profiles, with the default
config path shown by `agl config paths`. Packaged model selections are exposed
as agentFUNCTIONs, and initialized workspaces use a function by default:

```bash
agl init
agl function status gemma4-12b
agl chat
agl chat --function gemma4-12b
```

Check the currently active profile and repair hints with:

```bash
agl config status
agl config status --config /path/to/local.toml --strict
```

Use direct inference commands only when intentionally bypassing functions for a
backend smoke test or config repair:

```bash
agl inference chat --config /path/to/local.toml
```

Minimal profile shape:

```toml
[backend]
kind = "llama_cpp"
model = "/absolute/path/to/model.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"

[prompt]
# Optional skills injected by every command using this profile.
skills = ["repo-status"]
```

Function-backed `agl chat` loads this file through the selected function when
the chat session starts. Start a new chat after changing model/runtime fields
or `[prompt].skills`.
