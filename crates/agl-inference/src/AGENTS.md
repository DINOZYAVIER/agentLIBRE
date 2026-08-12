# AGENTS.md

This component owns host-safe backend traits, evidence writes, admission,
private engine transport, and subordinate process supervision. Native runtime
execution belongs in the constrained `llama-server` process; host modules must
not initialize or link llama.cpp, ggml, or Vulkan inference code.

Preserve evidence writes for every attempted inference path: request, response
or typed failure, bounded runtime log, and events. Engine exit before an
allocation receipt must remain explicit rather than fabricating allocation
data.
