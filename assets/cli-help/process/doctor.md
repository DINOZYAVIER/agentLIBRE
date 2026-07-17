Inspect the native process launcher and fail-closed workspace sandbox backend.
The report covers launcher availability, namespaces, Landlock, seccomp, pidfd,
and PTY support. Missing required Linux isolation primitives prevent workspace
targets from spawning; the runtime never silently weakens the sandbox.
