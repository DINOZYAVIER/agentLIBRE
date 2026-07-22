# AGENTS.md

This component owns host-safe backend traits, evidence writes, admission,
private worker IPC, and worker supervision. Native runtime execution belongs in
`agl-inference-worker`; host modules must not initialize or link llama.cpp,
ggml, or Vulkan inference code.

Preserve evidence writes for every attempted inference path: request, response
or typed failure, bounded runtime log, and events. Worker exit before an
allocation receipt must remain explicit rather than fabricating allocation
data.
