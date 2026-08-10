agentLIBRE CLI - local-first agentic system

Basics:
- .agl/workspace.toml declares package sources and the default function.
- Runtime Artifacts are verified Git submodules declared by installed Extensions.
- SKILL.md files add task-specific instructions and list the tools they may use.
- FUNCTION.md and SYSTEM.md files bind system prompt, profile, skills, tools,
  memory, and subagents.
- Core skills are trusted by the binary.
- Workspace packages resolve through .agl/package-lock.toml; non-core skills also need local approval.

Common commands:
  agl
  agl-terminal
  agl session list
  agl session new
  agl init --dry-run
  agl daemon status
  agl function list
  agl skill list --trusted-only
  agl run --prompt "Summarize this workspace"
  agl inference run --config /path/to/local.toml --prompt "Reply once."
