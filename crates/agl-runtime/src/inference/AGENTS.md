# AGENTS.md

This component owns the host-safe service API, bounded queue, codecs, private
engine transport and profile selection. systemd owns subordinate process
supervision.
Native runtime execution belongs in the constrained `llama-server` process;
host modules must not initialize or link llama.cpp, ggml or Vulkan inference
code.

Do not write inference-owned evidence or runtime files. Return structural
realization data with successful model results and emit only observed
`WorkerHealth`/`ResourceQuarantine` updates for Store persistence. Engine exit
before readiness or allocation evidence must remain explicit; never fabricate
allocation data.

## Module map

- `mod.rs`: public inference API exports.
- `service.rs`: model-service registration, bounded generation queue, cancellation and progress/health sinks.
- `llama_host.rs`: runtime profiles, private engine startup/readiness and generation transport coordination.
- `llama_host/wire.rs`: HTTP request construction, streaming transport, reasoning/schema adaptation and allocation validation.
- `llama_host/output.rs`: engine response/stream interpretation; `llama_host/tests.rs` checks host behavior.
- `engine_bundle.rs`: private engine/library bundle digest.
- `health.rs`: worker failures, resource quarantine and restored health values.
- `request_codec/mod.rs`: Agent-context projection into model requests.
- `output_codec/`: `parse.rs` decodes output, `action.rs` validates actions, `repair.rs` builds repair requests; `tests.rs` covers the codec.
