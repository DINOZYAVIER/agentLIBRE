Inspect and verify agentLIBRE skills.

Skill use:
- core skills ship with the binary and are trusted by the binary.
- workspace skills live under .agl/skills.
- SKILL.md embeds the common package identity and lists allowed tools, hooks, references, and guarantees.
- .agl/artifact-lock.toml records the exact workspace package identity and digest.
- local state/skill-trust.toml approves that exact identity and digest for --skill.

After editing a workspace skill:
  agl skill status
  agl repo component lock
  agl skill trust <name> --yes
  agl skill verify

Runtime visibility:
  agl run reloads skills on each invocation.
  bare agl can refresh selected skill context with /reload.
  Start a new interactive session after changing a daemon profile.
