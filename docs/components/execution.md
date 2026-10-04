# Execution

`agl-core::execution` owns ExecutionId, request, stream and outcome values.
The daemon reserves each AgentOperationKey in its Store before launch. Native
Tools use the daemon-supplied ExecutionHost; private inference uses ProcessHost.
There is no execution socket, separate execution database or execd product.

systemd user services own payload processes, timeouts and cgroups. Each
execution uses Type=exec, ExitType=main, KillMode=control-group and Restart=no.
Stop sends SIGTERM, then SIGKILL to remaining group members after two seconds.
Main-process completion also ends its remaining descendants. Exit/signal is
stored separately from the requested stop cause. Failed preparation/exec or
missing outcome evidence is unknown; a reserved Operation is never replayed.

An independent tmux server has its own user service and private socket.
Ordinary executions bind to that service and survive daemon restart or normal
Run completion. Explicit Run cancellation stops executions owned by that Run;
later Runs in the same Conversation may inspect/control them without taking
ownership. Other Conversations cannot access them. Loss of tmux stops their
cgroups. Git, planner and inference executions instead bind to the daemon unit.

`command.exec` actions are start, inspect, read, wait, stop and cleanup.
`terminal.session` additionally provides open, write, resize and interrupt.
Both use ExecutionId. Start/open are asynchronous; timeout_ms defaults to zero
(no process timeout), and wait has its own wait_timeout_ms. Cleanup removes
completed output files but retains the short Store record and Operation binding.

Command capture preserves separate raw stdout/stderr bytes. Terminal capture
uses tmux pipe-pane and a controlling PTY supplied to the systemd service.
Capture readiness gates payload execution, retaining its first output.
Each stream retains its first 16 MiB by default and continues draining excess
bytes; status reports bytes and truncated. Reads use stream-specific offsets.
Result files are published after outcome and capture completion under the data
root at executions/<ExecutionId>/. The daemon reconciles them into its Store.

Planner commands retain ReadOnlyWorkspace isolation: a separate reflink view
of workspace and external Git/commondir trees, mounted read-only by bubblewrap.
The declared output_directories trees are excluded; regular files, metadata and
symlink boundaries remain protected. Missing FICLONE support fails without a
copy fallback. The executable must resolve into the Nix store. The daemon
resolves bubblewrap from its PATH and passes the absolute path to the helper.
Host HOME, /run and /tmp are not mounted wholesale. Namespaces and seccomp
block network, host sockets and process inspection. Per-execution scratch is
writable and removed by ExecStopPost; recovery removes inactive orphan trees.
The scratch lives at executions/planner-scratch/<ExecutionId>/ below data.

Planner commands also share a persistent cache for their canonical workspace
under data/planner-cache/<workspace-path-sha256>/. The sandbox exposes it as
XDG_CACHE_HOME; output_directories remain excluded and commands choose their
own build target directory. AGL does not serialize cache access. CARGO_HOME
points to the cache's cargo/ directory, with the host Cargo registry/ and git/
mounted read-only when present. Host Cargo configuration and credentials are
not mounted, and CARGO_NET_OFFLINE=true. Seccomp rejects mknod/mknodat,
including FIFO creation in this writable cache. The cache is opened without
following a root symlink and mounted by its pinned descriptor, which is closed
before the command starts.
`agl cache clear [--workspace PATH]` removes that workspace's cache; the default
workspace is the current directory. Cache contents have no automatic TTL/quota.

Private inference retains pinned executable/model descriptors, a private
pre-opened listener, Landlock ABI 5, seccomp, device authority and its address
space bound. Lifecycle belongs to systemd; these isolation controls are not
replaced by cgroup ownership.

Service Git receives an otherwise empty environment, exact argv and private
stdin. Its absolute Tool deadline includes startup; cancellation/deadline
initiates the same two-second stop sequence. `[service_git] memory_max_bytes`
sets MemoryMax for the whole cgroup (default 1073741824); max_output_bytes sets
the capture bound per stream (default 1048576). Both are positive integers.
A started git apply with unproven completion remains OutcomeUnknown and is not
rolled back or retried automatically.

All daemon modes require a systemd user manager. `agl serve` from a shell runs
its daemon in a transient user service and waits; Ctrl+C/SIGTERM stops that
service. Installed socket activation starts the daemon directly as its own
service MainPID. The tmux socket is executions/tmux.sock below the state root.

Run live execution checks with the built agl helper and a user manager:

```sh
cargo build -p agl-cli --bin agl
scripts/agl-nix-vulkan.sh -- env AGL_TEST_HELPER="$PWD/target/debug/agl" \
  cargo test -p agl-daemon execution::tests::systemd -- --ignored --nocapture
```
