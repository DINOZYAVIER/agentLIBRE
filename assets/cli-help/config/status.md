Report the active agentLIBRE runtime and inference setup.

This checks the main runtime config, the local inference profile, resolved log
paths, session/store paths, and skill trust store. It does not start a model or
create missing files.

Use --config to inspect a specific local inference profile. Without --config,
the command follows the same default as run/chat: AGL_LOCAL_INFERENCE_CONFIG
first, then the default local.toml shown by agl config paths.

Common use:
  agl config status
  agl config status --config ~/.config/agentLIBRE/inference/local.toml
  agl config status --strict
