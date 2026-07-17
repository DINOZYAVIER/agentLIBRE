# Gemma 4 QAT CPU admission evidence

Date: 2026-07-15

Historical note: these measurements used 2,048-token contexts. They no longer
back active automatic profiles after the supported floor moved to 32,768.
Current per-model CPU admission records are dated 2026-07-16.

This record backs the initial CPU profiles in `assets/models/catalog.toml`.
Every run used the in-process agentLIBRE llama.cpp backend, `gpu_layers = 0`,
`context_tokens = 2048`, `batch_size = 128`, `ubatch_size = 64`, eight CPU
threads, Q8_0 KV caches, mmap, no swap, and a real generated answer. Required
projectors were loaded for E4B and 12B. Paths are intentionally omitted.

The main GGUF SHA-256 values were rechecked locally before the runs:

- E4B: `b3052f962d6449b4eb2075733c068bdec1c51eadb7b237e6c3157bfbb7b1dae0`
- 12B: `cc9ff072e0a8203429ed854e6662c17a6c2bc1e5dca5b475dd4736caaacbc165`
- 26B-A4B: `dcf179a91153e3a7ece792e48ef872180d9d6ef9b7677f0a0bd3e83cfe624d5e`
- 31B: `9188a71055550f1e60b875d02b7abb63625ac11b4a6f148d6b22b3b28ba3d335`

## Results

| Package | Memory limit | Output tokens | Wall time | Maximum RSS | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| E4B + 990 MB projector | 6 GiB | 16 | 7.57 s | 5,400,140 KiB | pass, no swap/OOM |
| E4B + 990 MB projector | 8 GiB | 16 | 7.61 s | 5,405,384 KiB | pass, no swap/OOM |
| 12B + 175 MB projector | 10 GiB | 4 | 17.25 s | 7,451,028 KiB | pass, no swap/OOM |
| 26B-A4B | 18 GiB | 4 | 24.98 s | 14,391,700 KiB | pass, no swap/OOM |
| 31B | 22 GiB | 4 | 41.25 s | 18,078,016 KiB | pass, no swap/OOM |

The limits were applied with a user-systemd cgroup and `MemorySwapMax=0`.
Unconstrained cold-cache comparison runs also passed. They reported 17.34 s /
7,940,800 KiB for E4B, 61.02 s / 13,985,632 KiB for 12B, 35.14 s /
28,150,952 KiB for 26B-A4B, and 124.10 s / 34,825,636 KiB for 31B.
File-backed mmap residency explains why the constrained steady-state peaks are
the admission values and the cold unconstrained RSS values are retained only as
diagnostic context.

## Admission values

- E4B: nominal 8-GB class, 6,000,000,000 currently available bytes.
- 12B: nominal 12-GiB class, 10,000,000,000 currently available bytes.
- 26B-A4B: nominal 20-GiB class, 16,000,000,000 currently available bytes.
- 31B: nominal 24-GiB class, 20,000,000,000 currently available bytes.

These are conservative rounded gates above the constrained maximum RSS and
include room for the bounded 2048-token runtime buffers. They do not claim that
larger contexts fit. The E4B profile is the only recommended default; the other
CPU profiles are intentionally labelled slow.
