# Execution

`agl-execd` is the agentLIBRE process and PTY authority. It serves a private
same-UID Unix socket, launches exact argument vectors without shell
interpretation, supervises process groups, and retains bounded output and
terminal outcomes in SQLite independently of client connections.

`agl-execution-api` contains the transport-neutral IDs, requests, outcomes,
JSONL protocol, and async client. Agent-owned requests identify their
Conversation and AgentRun; trusted runtime requests identify their internal
component.

`agl-daemon` admits `agentlibre.execution:command.exec` and
`agentlibre.execution:terminal.session` like every other Tool, then maps the
admitted call to the execution API. `command.exec` uses pipes and waits for a
bounded result. `terminal.session` opens or controls a managed PTY using
cursor-based reads, input, resize, interrupt, and terminate operations.
Reads include the current lifecycle state, terminal outcome, and valid next
actions. A control request for an exited Terminal is a typed no-effect Tool
result with the same lifecycle detail, so a model can read final output or open
a new Terminal without attempting to revive the old process.

The service can run in the foreground during development or claim its socket
from systemd. Its private launcher establishes the process-group and
controlling-terminal boundary. No terminal-emulator frontend participates in
this path.

Planner commands use `ExecutionIsolation::ReadOnlyWorkspace { workspace_root, output_directories }`,
which requires an Agent owner and pipes. Execd prepares a separate reflink view
of the workspace, its external Git directory and `commondir` for each Execution.
The view contains regular files, directories and symlinks; special source entries
fail preparation. File data, permissions and access/modification timestamps are
copied; source files retain their own inodes. Preparation uses the execution
timeout budget. Missing FICLONE support fails explicitly without copying file
contents through a fallback.

The daemon reads `[workspace] output_directories` from the root
`agentLIBRE.toml` and passes it through `ToolContext` and the execution request.
These literal relative directory trees are omitted from the planner view;
execd validates that the paths contain no traversal and cannot exclude the
workspace root or its manifest. Tools and execd do not parse the workspace TOML.
An excluded build cache is unavailable to planner commands, including through
workspace symlinks. Builds can use the existing per-Execution scratch.

The current profile requires Linux, bubblewrap on execd's PATH, and a canonical
Nix store executable. The sandbox exposes the view read-only, the trusted NAR
tree at `/nix/store`, Nix runtime executable/PATH aliases, and copies of
`/etc/passwd`, `group`, `nsswitch.conf`, `hosts` and `localtime` when present.
It creates fresh `/proc` and `/dev`. Host HOME, `/run` and `/tmp` are not mounted
as whole trees; only declared workspace/Git paths and Nix runtime aliases below
them are exposed. Workspace symlinks resolve against this limited view.
Host FIFO inodes are not shared through workspace bind mounts, and later host
entries do not appear in an already prepared view. Networking, host sockets,
host IPC and process-inspection syscalls are blocked by namespaces and seccomp.
Failure to launch bubblewrap never selects Standard execution.

Execd creates the private view and scratch under its data root at
`execd/planner-scratch/<ExecutionId>/`. Only the `scratch/` subtree is writable;
TMPDIR, TMP, TEMP and XDG_CACHE_HOME point there. Execution storage must lie
outside the copied source trees. Execd removes both view and scratch before
recording the terminal outcome, including timeout, and removes orphaned trees
during service recovery. The development Nix shell includes bubblewrap;
service deployments must also make it available on execd's PATH. Tool names
resolve through the daemon's PATH, so a name that selects an executable outside
the Nix store fails this profile.
