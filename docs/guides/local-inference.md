# Local Inference

Local inference uses GGUF files through llama.cpp profiles, with the default config path shown by `agl config paths`.

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
```
