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

Use `agl init --dry-run` to inspect the complete plan first. Machines with less
than 8 GB of physical RAM are rejected by default; `--allow-low-memory` permits
a best-effort attempt without claiming that the machine is supported.
