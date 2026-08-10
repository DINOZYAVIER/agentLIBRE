Inspect and verify agentLIBRE skills.

Skill use:
- core skills ship with the binary and are trusted by the binary.
- workspace skills are Skill packages resolved from WorkspaceManifest.sources.
- SKILL.md embeds the common package identity and lists allowed tools, hooks, references, and guarantees.
- .agl/package-lock.toml records the exact workspace package identity and digest.
- local state/skill-trust.toml approves that exact identity and digest for --skill.

After editing a workspace skill:
  agl package lock
  agl skill status
  agl skill trust <name> --yes
  agl skill verify

Runtime visibility:
  agl run reloads skills on each invocation.
  bare agl can refresh selected skill context with /reload.
  Start a new interactive session after changing its resolved package graph.
