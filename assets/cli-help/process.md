Inspect and control durable process executions owned by the local agentLIBRE
daemon. Process commands use the private same-user daemon socket; they do not
read SQLite or spool files directly and cannot discover process-local handles
owned by a separate direct-chat process.

Use `list`, `status`, or `read` for inspection, `attach` for a live terminal,
`kill` for explicit termination, and `doctor` to inspect Linux sandbox support.
Commands and terminal contents are private and are not shown by list/status
unless a command explicitly requests the bounded private display command.
