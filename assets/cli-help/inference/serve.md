Run the direct inference daemon in the foreground.

This command does not load an agentFUNCTION, workspace default function, or
function evidence. Use top-level `agl serve` for the default function-backed
daemon.

Common use:
  agl inference serve --config /path/to/local.toml
