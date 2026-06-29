---
name: memory-capture
description: Identify explicit memory candidates for agentLIBRE. Use when a conversation, decision, repo convention, Matrix room preference, or recurring workflow should be suggested for durable memory.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - secret_scan.validate
allowed_tools:
  - fs.read
  - fs.search
  - memory.suggest
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - memory capture must be suggested explicitly and not written silently
  - memory candidates must include scope, kind, title, body, and source reference
  - memory candidates must not include secrets or private local state
---

Use this skill to create pending durable memory suggestions through
`memory.suggest`.

Do not approve memory automatically. Each `memory.suggest` call creates a
pending suggestion that a user or trusted operator must later approve or reject.
Keep each candidate small, stable, and scoped: user, repo, Matrix room, or
Matrix user.

Prefer decisions, preferences, and durable facts over raw transcript snippets.
Do not promote credentials, tokens, private paths, or ephemeral debugging noise.
