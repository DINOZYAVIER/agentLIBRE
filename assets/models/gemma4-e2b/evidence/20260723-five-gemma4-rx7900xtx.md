# Five Gemma 4 RX 7900 XTX admission evidence

Date: 2026-07-23

This record backs the five automatic Vulkan profiles in
`assets/models/catalog.toml`. Direct measurements used llama.cpp b8983 at
commit `80afa33`, the `Vulkan0` AMD Radeon RX 7900 XTX (RADV NAVI31,
PCI `1002:744c`, subsystem `1da2:471e`), and a 24,560-MiB device. The clean
inventory reported about 22,938 MiB available before load.

Every tuple used all-layer offload (`gpu_layers = 999` at the agentLIBRE
boundary), Flash Attention, Q8_0 K and V caches, batch 512, ubatch 256, eight
CPU threads, `mmap = true`, `kv_unified = true`, one sequence, no MTP, and
llama.cpp fitting disabled. E4B loaded its catalog projector. The external
llama-server build rejected the 12B `gemma4uv` projector, so the 12B direct
measurement is a text-core receipt and its estimate retains a separate
projector/backend allowance. E2B, 26B-A4B, and 31B loaded no projector. E2B,
E4B, 26B-A4B, and 31B used 32,768 tokens; 12B used 65,536.

The main artifacts were checked before measurement:

- E2B official Q4_0: 3,349,514,112 bytes,
  `3646b4c147cd235a44d91df1546d3b7d8e29b547dbe4e1f80856419aa455e6fd`;
- E4B UD-Q4_K_XL: 4,215,693,760 bytes,
  `b3052f962d6449b4eb2075733c068bdec1c51eadb7b237e6c3157bfbb7b1dae0`;
- 12B UD-Q4_K_XL: 6,716,355,328 bytes,
  `cc9ff072e0a8203429ed854e6662c17a6c2bc1e5dca5b475dd4736caaacbc165`;
- 26B-A4B UD-Q4_K_XL: 14,249,045,120 bytes,
  `dcf179a91153e3a7ece792e48ef872180d9d6ef9b7677f0a0bd3e83cfe624d5e`;
- 31B official Q4_0: 17,651,001,568 bytes,
  `179cfb99212709597eae5929112cfca677e1bbf566178b479ae1da0c4772874b`.

The required projector identities were the E4B F16 projector at 990,372,672
bytes / `6a255159ee4b01b304f633a57f017dd7d5a69d30fff52abb2614bf0813cef034`
and the 12B F16 projector at 175,115,840 bytes /
`ecc4e93128da8363b7dbf2193eab98cf1142353f52ceaa0c95c0872997aaadd3`.

## Measured receipts and catalog gates

The receipt components below are rounded upward from the exact bundled-worker
buffers. The earlier direct llama.cpp runs established the initial envelopes;
the worker verification below is authoritative for promotion. The catalog
VRAM gate is the verifier estimate plus its 1,024-MiB desktop reserve; it is
not the observed free-memory delta.

| Package | Full-offload placement | Context | Receipt model / context / transient MiB | Estimate model / context / transient / uncertainty MiB | Catalog gate MiB |
| --- | --- | ---: | ---: | ---: | ---: |
| E2B | all model layers on Vulkan0 | 32,768 | 1,342 / 107 / 284 | 1,500 / 160 / 384 / 512 | 3,580 |
| E4B + projector | 43/43 model layers, projector on Vulkan0 | 32,768 | 3,438 / 288 / 517 | 3,600 / 384 / 640 / 512 | 6,160 |
| 12B + projector | 49/49 model layers, projector on Vulkan0 | 65,536 | 6,558 / 757 / 342 | 6,700 / 900 / 384 / 512 | 9,520 |
| 26B-A4B | 31/31 model layers on Vulkan0 | 32,768 | 13,574 / 473 / 263 | 13,850 / 600 / 384 / 512 | 16,370 |
| 31B | 61/61 model layers on Vulkan0 | 32,768 | 16,819 / 1,892 / 290 | 17,050 / 2,050 / 384 / 512 | 21,020 |

All five estimates exceed each corresponding receipt component, retain
512 MiB of tuple uncertainty and a separate 1,024-MiB desktop reserve, and fit
the measured clean-device snapshot. The 12B and E4B model receipts include
their projector weight allocations, and their transient receipts include the
projector compute buffers. The catalog stores the gate as exact MiB-to-byte
conversion.

## Isolated worker verification

The release `agl` and its exact sibling `agl-inference-worker` were built from
this task checkout. The worker selected native bundle
`sha256-8f69900b0b8dc70c5a3cf77efa00a9f03536381de93d12824c81ab20d88d2d1b`.
An isolated daemon used `/tmp/agl153-five-home` and never connected to the
installed user daemon. Each function completed a bounded one-shot generation:

| Function | Result | Full GPU placement | Context/KV |
| --- | --- | --- | --- |
| `gemma4-e2b` | `E2B_OK` | 36/36 layers | 32K, Q8_0/Q8_0 |
| `gemma4-e4b` | `E4B_OK` | 43/43 layers plus exact 944.39-MiB projector | 32K, Q8_0/Q8_0 |
| `gemma4-12b` | `B12_OK` | 49/49 layers plus exact 167.00-MiB projector | 64K, Q8_0/Q8_0 |
| `gemma4-26b` | `B26_OK` | 31/31 layers | 32K, Q8_0/Q8_0 |
| `gemma4-31b` | `B31_OK` | 61/61 layers | 32K, Q8_0/Q8_0 |

The bundled worker measurements, rather than the external text-only process,
are the promoted receipts in the table above. In particular, the 12B worker
accepted the exact catalog `gemma4uv`/`gemma4ua` projector that the external
llama-server build could not load. E4B accounts for one shared 944.39-MiB
projector weight allocation and its 101.27- and 153.93-MiB Vulkan compute
buffers. 12B accounts for one shared 167.00-MiB projector weight allocation
and its 18.03- and 58.59-MiB Vulkan compute buffers.

## CPU fallback provenance

CPU selection remains a separate explicit fallback. The existing E4B and 26B
profiles retain their dated CPU records. Official E2B completed a real 32K
generation with 3,551,452-KiB RSS. Its runtime reported 3,179.26 MiB of mapped
weights, 102 MiB of global Q8_0 KV, 4.78 MiB of SWA Q8_0 KV, and 12.44 MiB of
host compute. The Vulkan-enabled build also allocated 384.13 MiB of device
compute scratch at `gpu_layers = 0`. This supports the catalog's nominal
8-GB / 6,000,000,000-available-byte fallback gate without claiming zero GPU
use.

The 12B 64K and official Google 31B 32K CPU fallback tuples were re-run with
the same llama.cpp b8983 / `80afa33` build. Both used `gpu_layers = 0`,
`mmap = true`, `kv_unified = true`, Flash Attention, Q8_0 K/V, batch 128,
ubatch 64, eight threads, and fitting disabled. Each completed a 21-token
prompt plus 32 generated tokens:

| Package | Context | CPU model MiB | CPU global / SWA KV MiB | Host compute MiB | Host total MiB | Maximum RSS KiB | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 12B text core | 65,536 | 6,390.13 | 544 / 212.50 | 19.56 | 7,166 | 7,518,620 | pass |
| official 31B | 32,768 | 16,818.21 | 1,360 / 531.25 | 12.93 | 18,722 | 19,359,608 | pass |

These are CPU weight and KV placements, not claims of zero GPU use. With the
Vulkan backend active, llama.cpp still allocated 641.27 MiB of Vulkan compute
scratch for 12B and 1,259.38 MiB for 31B even at `gpu_layers = 0`. The catalog
CPU profiles therefore describe model offload placement and host admission;
they do not promise that a Vulkan-enabled build leaves the device untouched.
The measured host totals and RSS remain below the catalog's 12B nominal
16-GiB / 14,000,000,000-available-byte and 31B nominal 40-GiB /
40,000,000,000-available-byte gates.
