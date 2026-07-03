---
status: planned
---

# Safe Benchmark Procedure

## Change

Add a measured no-MTP context ladder for Gemma4 profiles. Do not run 256k from a
GUI editor session.

## Ladder

Run small-output smokes and collect `runtime.log`, `response.json`, wall time,
RSS, selected device, model buffer, KV buffer, and compute buffer.

1. 12B, 32k, f16 KV.
2. 12B, 64k, `cache_type_k = "q8_0"`, `cache_type_v = "q8_0"`.
3. 12B, 128k, `cache_type_k = "q4_0"`, `cache_type_v = "q4_0"`.
4. 12B, 256k only with explicit approval, from a non-GUI session, and only
   after deciding whether to reduce `gpu_layers` or accept CPU/offload speed.

## Guardrails

- Keep `runtime.mtp.enabled = false`.
- Use a temporary profile first; do not overwrite production profiles until the
  smoke passes.
- Prefer a TTY, systemd service, or detached shell that does not host the editor
  session.
- Use low `max_output_tokens` for initialization smokes.
- Stop after the first OOM, Vulkan device loss, desktop instability, or missing
  `runtime.log`.

## Production Profile Rule

Only promote a context size into `~/.config/agentLIBRE/inference/profiles` after
the same size and KV cache type has a successful artifact-backed smoke.
