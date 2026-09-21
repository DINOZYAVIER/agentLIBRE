# AGENTS.md

This component owns the host-safe service API, bounded queue, codecs, private
engine transport and profile selection. `agl-execd` owns subordinate process
supervision.
Native runtime execution belongs in the constrained `llama-server` process;
host modules must not initialize or link llama.cpp, ggml or Vulkan inference
code.

Do not write inference-owned evidence or runtime files. Return structural
realization data with successful model results and emit only observed
`WorkerHealth`/`ResourceQuarantine` updates for Store persistence. Engine exit
before readiness or allocation evidence must remain explicit; never fabricate
allocation data.
