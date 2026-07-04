# Gemma4 Inference Profiles

These profiles record the local Gemma4 QAT q8 KV-cache ceilings measured on the
24GB Vulkan0 workstation.

Default local inference config:

```bash
cp assets/inference/profiles/gemma4-12b-q8-ceiling.toml \
  ~/.config/agentLIBRE/inference/local.toml
chmod 600 ~/.config/agentLIBRE/inference/local.toml
```

Profile install:

```bash
mkdir -p ~/.config/agentLIBRE/inference/profiles
cp assets/inference/profiles/gemma4-*.toml \
  ~/.config/agentLIBRE/inference/profiles/
```

The default is Gemma4 12B q8 KV at 96k context. The 26B and 31B profiles are
available explicitly, but they leave less VRAM headroom.
