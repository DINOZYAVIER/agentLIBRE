Run one prompt and print the final answer.

Use run for one-shot prompts. It loads the workspace default agentFUNCTION from
.agl/workspace.toml unless --function selects another function. Add --skill to
include a core or trusted workspace skill, and --tool-mode to choose filesystem
access. Use --json for a machine-readable result or nested error with durable
run and runtime-resolution evidence identifiers.

Common use:
  agl run --prompt "Summarize this workspace"
  agl run --function coding --prompt "Summarize this workspace"
