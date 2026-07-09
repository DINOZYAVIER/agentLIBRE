Run one direct inference prompt and print the final answer.

This command does not load an agentFUNCTION, workspace default function, or
function evidence. Pass --config or rely on the active local inference config.

Common use:
  agl inference run --config /path/to/local.toml --prompt "Reply once."
