Validate one agentFUNCTION without running inference.

The status command checks the function manifest, system prompt, function-owned
inference preset or referenced inference profile, declared subagent files, and
model ids resolved through `$AGL_HOME/config/models.toml`. It reports missing
bindings without starting a model process.

Examples:
  agl function status coding
  agl function status coding --strict
  agl function status coding --json
