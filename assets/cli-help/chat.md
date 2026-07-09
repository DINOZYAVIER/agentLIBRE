Start an interactive chat session.

Use chat when you want multiple turns in one session. The session transcript is
saved under the AgentLIBRE data directory unless --no-history is used.

Common use:
  agl chat --config /path/to/local.toml --skill repo-status
  agl chat --function coding

Inside chat, use /session to print artifact and workspace paths, /workspace to
change the filesystem root, and /reload to refresh selected skill context and
visible tools, function manifest, system prompt, and subagent registry. The
local inference config and model are loaded when the chat session starts; start
a new chat or run command after changing --config, function model.config,
function model.profile, or the profile TOML.
