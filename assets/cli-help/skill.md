Inspect and verify agentLIBRE skills.

Skill use:
- core skills ship with the binary and are trusted by the binary.
- workspace skills live under .agl/skills.
- SKILL.md lists the skill name, source, allowed tools, hooks, references, and guarantees.
- .agl/skills.lock records the current workspace skill git commit.
- local state/skill-trust.toml approves that exact commit for --skill.

After editing a workspace skill:
  agl skill status
  agl skill lock
  agl skill trust <name> --yes
  agl skill verify

Runtime visibility:
  agl run reloads skills on each invocation.
  agl chat can refresh selected skill context with /reload.
  Start a new chat after changing --config or [prompt].skills in a profile.
