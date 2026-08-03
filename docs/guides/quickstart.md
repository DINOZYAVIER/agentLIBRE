# Quickstart

Install the CLI, let guided setup choose and verify a local model, then start a
chat:

```bash
scripts/install-agl-cargo.sh
agl init
scripts/agentlibre-daemon-systemd-service.sh --enable --restart
agl
```

`agl init` defaults to the conservative Gemma 4 E4B QAT Q4 package, including
its required vision projector. It inspects RAM, disk, and available inference
devices before downloading into the standard Hugging Face cache. If setup is
interrupted, run the same command again; it resumes the recorded package and
revalidates completed work.

The builtin choices are `gemma4-e2b`, `gemma4-e4b`, `gemma4-12b`,
`gemma4-26b`, and `gemma4-31b`. E2B, E4B, and 26B use a 32K profile; 12B uses
64K. The official 31B QAT Q4_0 package supplies reviewed 32K and 64K GPU
profiles, selected explicitly at run time by `gemma4-31b-32k` or
`gemma4-31b-64k`. Select model weights with `agl init --model MODEL_ID`.

Use `agl init --dry-run` to inspect the complete plan first. Machines with less
than 8 GB of physical RAM are rejected by default; `--allow-low-memory` permits
a best-effort attempt without claiming that the machine is supported.
