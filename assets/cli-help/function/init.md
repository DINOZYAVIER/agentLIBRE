Create a starter agentFUNCTION.

By default, init writes to the global AgentLIBRE config directory. Use
--workspace to write under the current workspace .agl/functions tree.
It creates FUNCTION.md, SYSTEM.md, and subagents/.

Examples:
  agl function init coding --workspace
  agl function init coding --workspace --model-profile local
