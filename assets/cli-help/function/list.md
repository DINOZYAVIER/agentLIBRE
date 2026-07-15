List workspace, global, and builtin agentFUNCTIONs.

The list command scans the current workspace first, then the agentLIBRE config
directory, then builtin assets. It reports each function id, source, path, and
parse status.

Examples:
  agl function list
  agl function list --json
