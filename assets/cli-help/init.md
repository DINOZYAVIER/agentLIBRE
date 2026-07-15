Initialize a local agentLIBRE workspace.

This runs the repo workspace initializer, writes the workspace default function
in .agl/workspace.toml, creates the local function workspace root at
.agl/functions, and reports packaged builtin functions that are ready to
inspect or run.

It does not declare or fetch workspace artifacts by default. Use
install-hooks and skill init explicitly for those operations. Start with
--dry-run to see planned local changes.
