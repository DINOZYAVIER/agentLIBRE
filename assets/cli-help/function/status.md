Validate one agentFUNCTION without running inference.

The status command checks the function manifest, system prompt, function-owned
inference config or referenced inference profile path, declared subagent files,
and the configured GGUF model path. It does not start a model process.

Examples:
  agl function status coding
  agl function status coding --strict
  agl function status coding --json
