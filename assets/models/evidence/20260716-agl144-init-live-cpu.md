# AGL-144 normal-path CPU setup regression

Date: 2026-07-16

The opt-in `scripts/agl-init-live-smoke.sh` acceptance harness passed against
the release `agl` binary in CPU-required, fully offline mode. The workspace and
AGL home were isolated; the standard Hugging Face cache supplied the already
verified E4B main model and required projector.

The selected runtime was `cpu-8gb-2048` with `gpu_layers = 0`, context 2048,
batch 128, ubatch 64, eight threads, Q8_0 KV caches, and mmap. The plan reported
zero bytes to download. SHA-256 verification matched the embedded catalog:

- main: `b3052f962d6449b4eb2075733c068bdec1c51eadb7b237e6c3157bfbb7b1dae0`
- projector: `6a255159ee4b01b304f633a57f017dd7d5a69d30fff52abb2614bf0813cef034`

## Results

| Case | Exit | Wall time | Maximum RSS | Result |
| --- | ---: | ---: | ---: | --- |
| dry-run plan | 0 | 0.01 s | 33,732 KiB | CPU profile and complete cache selected |
| first init | 0 | 20.56 s | 7,874,100 KiB | binding staged, normal-path smoke passed, then committed |
| main verification | 0 | 2.52 s | 27,688 KiB | size, digest, and GGUF validation passed |
| projector verification | 0 | 0.59 s | 27,664 KiB | size, digest, and GGUF validation passed |
| normal function generation | 0 | 16.22 s | 7,868,752 KiB | returned `AGL144_TEXT_OK` |
| native Gemma tool fixture | 0 | 18.31 s | 7,869,296 KiB | read fixture and returned `AGL144_TOOL_OK` |
| active unbind | 1 | 0.00 s | 28,104 KiB | refused because the model remained active |
| completed offline re-entry | 0 | 19.30 s | 7,872,628 KiB | fresh smoke passed with no transfer |

The harness completed with `result=passed`. This run is regression evidence,
not the admission benchmark: the constrained no-swap CPU measurements remain
in `20260715-gemma4-qat-cpu.md`, and the separately measured Vulkan profiles
remain in `20260715-gemma4-qat-vulkan.md`.
