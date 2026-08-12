# Local Inference

agentLIBRE runs the GGUF files it needs directly. Guided setup is the normal
path:

```bash
agl init
agl
```

Native llama.cpp, ggml, and Vulkan state runs in the exact private constrained
`llama-server` child supervised by `InferenceHost`. A native abort, signal,
protocol failure, or lost GPU therefore fails the active attempt without taking
down the daemon, durable session, or Human terminal. The failed engine is reaped
and the device enters a bounded cooldown; only a later explicit request may
start a clean engine after fresh admission. `agl daemon status` reports the
daemon identity plus the engine build, PID/FSM state, selected device,
reservation, and cooldown without exposing prompts or native payloads.

The conservative default is `gemma4-e4b`, a Gemma 4 E4B QAT Q4 main model plus
its required projector. `--model gemma4-e2b` selects the text-and-tools
official QAT E2B package without a projector; `gemma4-12b`, `gemma4-26b`, and
`gemma4-31b` select larger pinned packages, with 31B using the official QAT
Q4_0 artifact. E2B, E4B, and 26B have a 32K profile; 12B has 64K. The 31B
package has separate reviewed 32K and 64K GPU profiles selected by the
`gemma4-31b-32k` and `gemma4-31b-64k` Functions.
The plan reports cache hits, bytes to download, the measured runtime profile,
numeric context/batch/thread/offload values, binding/default changes, and the
required smoke before it writes.

Setup state is resumable per workspace. A repeated `agl init` keeps the
confirmed package, rechecks files and digests, continues from the last durable
phase, and commits `models.toml` plus the workspace default only after a normal
chat/model-manager smoke succeeds. A completed setup is inspected and smoked
again rather than reset.

Machines below the recommended 8 GB physical-memory class stop before
acquisition. `agl init --allow-low-memory` permits a best-effort attempt; it
does not turn the host into a benchmarked/supported profile. CPU-only machines
are supported when the selected Function names a measured CPU profile. A
rejected or failed GPU plan never falls back within the attempt; selecting a
CPU profile is a new explicit Function/package resolution. The CPU backend
ships in the same build, while Vulkan and other
accelerator backends load dynamically when present; starting the CLI does not
require a Vulkan loader.

## Hugging Face acquisition

Remote acquisition uses the Rust `hf-hub` client and the standard Hugging Face
cache. Standard `HF_TOKEN`, `HF_HOME`, `HF_HUB_CACHE`, `HF_ENDPOINT`, and
`HF_HUB_OFFLINE` configuration apply; agentLIBRE does not persist credentials.
Set `HF_HUB_OFFLINE=1` or pass `--offline` to prohibit Hub network access.

For a model outside the first-party pinned catalog, a normal repository link is
enough in a terminal:

```bash
agl model pull https://huggingface.co/owner/repository
```

The chooser lists exact GGUF candidates. Automation must use an exact blob or
resolve-file URL and explicit consent:

```bash
agl model pull \
  https://huggingface.co/owner/repository/resolve/COMMIT/model.gguf \
  --id my-model --yes --non-interactive
```

Use `--mmproj URL` when a custom model needs a projector; neighboring files are
never guessed. `agl model import /absolute/path/model.gguf --id my-model`
registers an existing local file without copying or scanning directories.
`model unbind`, `remove`, and `prune` are separate confirmed operations. Prune
only touches tombstoned, agentLIBRE-provenanced HF cache pointers/blobs and
honors active bindings, setup state, downloads, shared revisions, and loaded
model leases.

## Diagnostics and package-bound profiles

Check the currently active profile and repair hints with:

```bash
agl config status
agl function doctor function:gemma4-e4b
agl model status
```

There is no local fixed-profile TOML or raw runtime-argument override. A
Function selects one measured profile declared by its resolved Model v3
package; `agl-model` freezes that profile into an opaque execution plan and
`InferenceHost` applies it exactly. Future inference experiments extend the
typed Function generation policy or Model profile schema instead of adding an
argv/JSON escape hatch. Bare `agl`, `agl run`, and `agl serve` all use this one
path.
