agentLIBRE CLI - local-first agentic system

Basics:
- .agl/workspace.toml lists the repo's AgentLIBRE folders.
- .agl folders are checked against that file before agl reads or writes them.
- SKILL.md files add task-specific instructions and list the tools they may use.
- FUNCTION.md and SYSTEM.md files bind system prompt, profile, skills, tools,
  memory, and subagents.
- Core skills are trusted by the binary.
- Workspace skills need .agl/skills.lock and local approval before --skill can use them.

Common commands:
  agl init --dry-run
  agl status
  agl function list
  agl skill list --trusted-only
  agl run --skill repo-status --prompt "Summarize this workspace"
