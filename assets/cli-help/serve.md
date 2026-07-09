Run the local agent runtime daemon in the foreground.

The daemon serves local client requests over a Unix socket. It loads the
workspace default agentFUNCTION from .agl/workspace.toml unless --function
selects another function. Logs are written under the AgentLIBRE state
directory.
