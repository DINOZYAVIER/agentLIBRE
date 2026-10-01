# AGENTS.md

`agl-daemon` is the Agent service implementation and composition root. Keep
Agent scheduling under `agent`, durable repositories under `store`, and
native Tool composition under `tools`. Concrete first-party handlers belong
to `agl-tools`; the native SDK belongs to `agl-tool-api`.

Tool handlers must validate their own host safety boundary. Registering a
handler does not admit it to an AgentRun; admission remains tied to the exact
Function/Run snapshot and Effect grants.

Do not expose scheduler or store implementation through the public daemon API.
Do not create child processes or PTYs here; execution is delegated through
`agl-execution-api` to `agl-execd`.
