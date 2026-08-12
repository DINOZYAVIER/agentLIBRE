Report the active agentLIBRE runtime setup.

This checks the main runtime config, resolved log paths, session/store paths,
and skill trust store. Model runtime profiles come only from resolved Model
packages and are reported by `agl function doctor` and `agl model status`.
The command does not start a model or create missing files.

Common use:
  agl config status
  agl config status --strict
