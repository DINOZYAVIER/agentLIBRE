# Inference Profile Assets

Standalone profile assets in this directory must not contain host-specific
model paths.

Put local profiles under the runtime config directory instead:

```bash
mkdir -p ~/.config/agentLIBRE/inference/profiles
install -m 600 local-profile.toml ~/.config/agentLIBRE/inference/local.toml
```

Use `agl config paths` to confirm the active config root for the current
`AGL_HOME` or XDG setup.

Portable profile examples can live here only when they use placeholders or
relative paths that are valid across machines.

Default model/runtime selections should be packaged as agentFUNCTIONs under
`assets/functions/<id>/` instead of standalone profile files. A function owns
its `FUNCTION.md`, `SYSTEM.md`, and `inference.toml` as one inspectable unit;
those function configs may describe the current local operational model layout.
