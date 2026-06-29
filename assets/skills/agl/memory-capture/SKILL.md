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
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - memory capture must be suggested explicitly and not written silently
  - memory candidates must include scope, kind, title, body, and source reference
  - memory candidates must not include secrets or private local state
---

Use this skill to propose durable memory entries.

Do not write memory automatically. Produce explicit candidates that a user or
trusted tool can approve first. Keep each candidate small, stable, and scoped:
user, repo, Matrix room, or Matrix user.

Prefer decisions, preferences, and durable facts over raw transcript snippets.
Do not promote credentials, tokens, private paths, or ephemeral debugging noise.
