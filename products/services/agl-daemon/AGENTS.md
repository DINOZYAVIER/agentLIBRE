# AGENTS.md

`agl-daemon` is the Agent service implementation and composition root. Keep
Agent scheduling under `agent`, durable repositories under `store`, and
concrete first-party handlers under `tools`.

Tool handlers must validate their own host safety boundary. Registering a
handler does not admit it to an AgentRun; admission remains tied to the exact
Function/Run snapshot and Effect grants.

Do not expose scheduler or store implementation through the public daemon API.
Do not create child processes or PTYs here; execution is delegated through
`agl-execution-api` to `agl-execd`.
