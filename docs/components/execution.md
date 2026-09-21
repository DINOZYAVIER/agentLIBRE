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
