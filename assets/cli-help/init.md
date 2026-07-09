Initialize a local AgentLIBRE workspace.

This runs the repo workspace initializer, creates the local function workspace
root at .agl/functions, and reports packaged builtin functions that are ready
to inspect or run.

It does not install git hooks or fetch submodules by default. Use
install-hooks and skill init explicitly for those operations. Start with
--dry-run to see planned local changes.
