---
name: repo-status
description: Inspect an agentLIBRE repository before or after work. Use when checking workspace health, changed files, relevant specs, recent commits, generated artifacts, or next-step readiness.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - verification.validate
allowed_tools:
  - fs.read
  - fs.list
  - fs.search
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - status summaries must distinguish source changes, generated artifacts, and local-only state
  - status summaries must include actionable next steps when the workspace is unhealthy
---

Use this skill when the agent needs a concise repository state picture.

Inspect the workspace manifest, task specs, touched crates, and recent local
changes before recommending a next step. Keep the result short and operational:
what changed, what is healthy, what is blocked, and what command or review
should run next.

Do not infer trust from copied files. Workspace skill trust remains local and
must be checked through AgentLIBRE status/trust commands.
