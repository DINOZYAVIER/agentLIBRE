# llama.cpp Runtime

AGL-012 PR5 makes llama.cpp an explicit project-owned runtime boundary.

The managed source checkout is:

```text
vendor/llama.cpp
```

Initialize or update it with:

```sh
git submodule update --init --recursive vendor/llama.cpp
```

Build the Vulkan completion binary out of tree:

```sh
scripts/build-llama-cpp.sh
```

On Nix systems without Vulkan/SPIR-V development files in the default shell,
use:

```sh
nix shell \
  nixpkgs#vulkan-headers \
  nixpkgs#vulkan-loader \
  nixpkgs#shaderc \
  nixpkgs#spirv-headers \
  nixpkgs#spirv-tools \
  nixpkgs#glslang \
  nixpkgs#cmake \
  nixpkgs#ninja \
  nixpkgs#gcc \
  -c scripts/build-llama-cpp.sh
```

The script writes build artifacts under:

```text
target/llama-cpp/build
```

The expected smoke binary is:

```text
target/llama-cpp/build/bin/llama-completion
```

## Qwen3.6 Smoke Shape

Use a local inference config whose backend binary points at the project-owned
build output:

```toml
[backend]
kind = "llama_cpp"
binary = "/home/dinozyavier/repos/agentLIBRE/target/llama-cpp/build/bin/llama-completion"
model = "/home/dinozyavier/.dyno/models/Qwen3.6-27B-UD-Q4_K_XL/Qwen3.6-27B-UD-Q4_K_XL.gguf"

[runtime]
gpu_layers = 999
context_tokens = 2048
threads = 8
device = "Vulkan0"
batch_size = 1024
ubatch_size = 256
flash_attention = "on"
cache_type_k = "q8_0"
cache_type_v = "q8_0"
mmap = false
jinja = true
conversation = false
simple_io = true
display_prompt = false

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
```

Run the ignored smoke test with an explicit artifact root:

```sh
AGL_LOCAL_INFERENCE_CONFIG=/home/dinozyavier/repos/agentLIBRE/.dyno/smoke/AGL-012/local-inference.toml \
AGL_INFERENCE_ARTIFACT_ROOT=/tmp/agentlibre-inference-smoke-AGL-012-managed \
cargo test -p agl-inference manual_llama_cpp_smoke_from_env -- --ignored --nocapture
```

The smoke path must not use `../agentLIBRE-legacy`. Legacy can be a diagnostic
comparison only.
