# AGENTS.md

This worker-only component owns the native llama.cpp runtime.
Keep FFI bindings aligned with the pinned vendored llama.cpp C API and preserve evidence writes for every attempted inference path.
