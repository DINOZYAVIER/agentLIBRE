# Local Inference

Local inference uses GGUF files through llama.cpp profiles, with the default config path shown by `agl config paths`.
Packaged model selections are exposed as agentFUNCTIONs, for example:

```bash
agl function status gemma4-12b
agl chat --function gemma4-12b
```

Check the currently active profile and repair hints with:

```bash
agl config status
agl config status --config /path/to/local.toml --strict
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
# Optional skills injected by every run/chat using this profile.
skills = ["repo-status"]
```

`agl chat` loads this file when the chat session starts. Start a new chat after
changing model/runtime fields or `[prompt].skills`.
