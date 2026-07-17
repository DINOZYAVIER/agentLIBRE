# Gemma 4 QAT Vulkan admission evidence

Date: 2026-07-15

Historical note: these measurements used 2,048-token contexts. They do not
back active 32K profiles. The Vulkan profiles were removed from the supported
catalog until a physical GPU passes the 32,768-token matrix.

This record backs the initial discrete-GPU profiles in
`assets/models/catalog.toml`. Runs used the in-process agentLIBRE llama.cpp
backend on an AMD Radeon RX 7900 XTX (RADV NAVI31), reported as 24,560 MiB
total and about 22,638 MiB free before model load. The runtime used a 2048-token
context, batch 128, ubatch 64, eight CPU threads, Q8_0 KV caches, mmap, and the
explicit `Vulkan0` device. Required projectors were loaded for E4B and 12B.

Initial discovery used an intentionally oversized direct-test request, then the
persisted runtime logs were used to record the exact full-offload layer count.
The catalog contains those exact counts, never an unlimited sentinel.

| Package | Exact GPU layers | Model buffer | VRAM used from inventory | Output tokens | Wall time | Maximum RSS | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| E4B + 990 MB projector | 43 | 2,493.31 MiB | 3,828 MiB | 16 | 4.17 s | 4,370,564 KiB | pass |
| 12B + 175 MB projector | 49 | 6,390.13 MiB | 7,168 MiB | 8 | 5.31 s | 6,813,468 KiB | pass |
| 26B-A4B | 31 | 13,573.86 MiB | 13,882 MiB | 8 | 6.23 s | 14,170,452 KiB | pass |
| 31B | 61 | 16,471.71 MiB | 17,482 MiB | 8 | 8.94 s | 17,137,280 KiB | pass |

All four runs generated non-empty text and runtime evidence named `Vulkan0` as
the selected device. The rounded catalog VRAM gates add headroom above the
observed inventory deltas. The CPU evidence remains a separately measured
fallback and is not inferred from these GPU runs.
