# AGENTS.md

This directory contains third-party vendored dependencies.
Currently `llama.cpp` is a git submodule pinned for the managed local inference runtime; update the submodule pointer intentionally and do not edit upstream files in place.

## Dependency map

`llama.cpp/` contains the engine (`src/`), tensor backends (`ggml/`), common
request/template/parser support (`common/`), GGUF utilities (`gguf-py/`) and
the HTTP engine (`tools/server/`). Its own AGENTS.md belongs to the submodule.
AGL builds it through `scripts/build-llama-cpp.sh` and accesses the resulting
private process through `agl-runtime::inference` and the daemon systemd adapter.
