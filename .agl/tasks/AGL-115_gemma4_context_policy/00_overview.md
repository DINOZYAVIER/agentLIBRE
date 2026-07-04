---
status: planned
---

# Gemma4 No-MTP Context Policy

## Problem

The Gemma4 QAT profiles currently use `context_tokens = 8192`. That is too
small for the intended agent workflow, but jumping directly to 256k with the
default f16 KV cache is unsafe on the current 24GB GPU workstation.

A 12B/256k/f16 smoke attempt on 2026-07-03 killed the graphical session before
`runtime.log` or `response.json` could be written. The only recorded artifacts
were the request and early attempt events.

## Goal

Define context sizes that are normal for agentLIBRE Gemma4 profiles without
MTP, and keep oversized experiments behind an explicit safe-benchmark path.

## Policy

- `32768` tokens is the minimum normal context for Gemma4 inference profiles.
- MTP is disabled for context sizing and production profile decisions.
- 12B/256k is not a default GPU profile on a 24GB card unless memory evidence
  proves it can initialize and generate without desktop instability.
- 12B/128k with quantized KV cache is the practical high-context GPU target.
- 12B/256k is experimental/offload-only until a safe headless benchmark proves
  it is usable.
- 26B and 31B should stay at 32k until separate no-MTP memory and speed
  evidence supports larger contexts.

## Evidence

Existing 12B no-MTP logs at `n_ctx = 8192` show:

- model file: `6.24 GiB`
- KV cache: `128 MiB + 2560 MiB = 2688 MiB`
- compute buffer: about `264.50 MiB`

Linear KV scaling gives this approximate 12B target-only footprint:

| Context | f16 KV | q8_0 KV | q4_0 KV | GPU profile status |
| --- | ---: | ---: | ---: | --- |
| 32k | 10.5 GiB | 5.25 GiB | 2.63 GiB | normal minimum |
| 64k | 21.0 GiB | 10.5 GiB | 5.25 GiB | test with q8_0/q4_0 |
| 128k | 42.0 GiB | 21.0 GiB | 10.5 GiB | q4_0 target |
| 256k | 84.0 GiB | 42.0 GiB | 21.0 GiB | unsafe on full GPU |

The 256k q4_0 estimate still leaves too little VRAM once the 6.24 GiB model
and compute buffers are included. It should not be launched as a fully offloaded
desktop GPU smoke.

## 2026-07-03 Smoke Result

The updated 12B profile was smoke-tested with:

- `context_tokens = 131072`
- `cache_type_k = "q4_0"`
- `cache_type_v = "q4_0"`
- `flash_attention = "on"`
- `runtime.mtp.enabled = false`

Artifact path:
`/tmp/agl-gemma4-context-bench/inference-runs/gemma4-12b-128k-q4-no-mtp-smoke`

Observed runtime values:

- `n_ctx = 131072`
- `llama_context: flash_attn = enabled`
- model file: `6.24 GiB`
- KV cache: `576 MiB + 11520 MiB = 12096 MiB`
- compute buffer: `264.50 MiB` GPU and `136.81 MiB` host
- selected device: `Vulkan0`
- duration: `5720 ms`
- wall time: `5.92s`
- max RSS: `6780960 KiB`

This makes 12B/128k/q4_0 the current verified high-context GPU profile.

## 2026-07-03 Q8 Ceiling Smoke

Temporary q8_0 KV profiles were smoke-tested to find the practical full-offload
ceiling for the 24GB Vulkan0 GPU. Each run used Flash Attention, no MTP, and a
short output limit.

| Model | Passed q8_0 context | GPU model buffer | GPU KV buffer | GPU compute buffer | Core GPU buffers | Headroom vs 24560 MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Gemma4 12B | 98304 | 6390.13 MiB | 17136.00 MiB | 264.50 MiB | 23790.63 MiB | 769.37 MiB |
| Gemma4 26B A4B | 90112 | 13573.86 MiB | 10285.00 MiB | 264.25 MiB | 24123.11 MiB | 436.89 MiB |
| Gemma4 31B | 16384 | 16471.71 MiB | 7480.00 MiB | 266.50 MiB | 24218.21 MiB | 341.79 MiB |

Artifact roots:

- `/tmp/agl-gemma4-q8-ceiling/inference-runs/gemma4-12b-96k-q8-ceiling-smoke`
- `/tmp/agl-gemma4-q8-ceiling/inference-runs/gemma4-26b-88k-q8-ceiling-smoke`
- `/tmp/agl-gemma4-q8-ceiling/inference-runs/gemma4-31b-16k-q8-ceiling-smoke`

Estimated hard ceilings with no desktop safety margin are roughly:

- 12B: about 100k q8_0 context.
- 26B A4B: about 92k q8_0 context.
- 31B: about 16k-17k q8_0 context.

The practical q8_0 ceilings for this workstation are the passed values above:
96k for 12B, 88k for 26B A4B, and 16k for 31B. Higher values leave too little
headroom for a graphical desktop and should not become normal profiles.

## Acceptance Criteria

1. Gemma4 profile updates use `32768` as the minimum normal context.
2. Any 12B profile above 32k explicitly sets target KV cache type.
3. MTP remains disabled for these profiles.
4. A 12B 128k q4_0 profile is smoke-tested before being treated as usable.
5. 12B 256k is only tested through the safe benchmark procedure in
   `01_safe_benchmark.md`.
