Inspect and create agentFUNCTION artifacts.

agentFUNCTIONs bind a system prompt, inference config, skills, tools, memory
policy, and subagent artifacts into one inspectable directory.

Common use:
  agl function init coding --workspace
  agl function status coding
  agl function show coding
  agl chat --function coding

Workspace functions live under .agl/functions/<id>/ with FUNCTION.md and
SYSTEM.md. Global functions live under the agentLIBRE config directory.
