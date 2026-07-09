Run one prompt and print the final answer.

Use run for one-shot prompts. It loads the workspace default agentFUNCTION from
.agl/workspace.toml unless --function selects another function. Add --skill to
include a core or trusted workspace skill, and --tool-mode to choose filesystem
access.

Common use:
  agl run --prompt "Summarize this workspace"
  agl run --function coding --prompt "Summarize this workspace"
