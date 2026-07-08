# Inference Profile Assets

Repo assets must not contain host-specific model paths.

Put local profiles under the runtime config directory instead:

```bash
mkdir -p ~/.config/agentLIBRE/inference/profiles
install -m 600 local-profile.toml ~/.config/agentLIBRE/inference/local.toml
```

Use `agl config paths` to confirm the active config root for the current
`AGL_HOME` or XDG setup.

Portable profile examples can live here only when they use placeholders or
relative paths that are valid across machines.
