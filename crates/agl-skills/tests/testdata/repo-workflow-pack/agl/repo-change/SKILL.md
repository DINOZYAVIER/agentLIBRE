---
name: repo-change
description: Synthetic workspace skill used to validate repo workflow pack parsing.
version: 1
source: local
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees:
  - fixture skill stays intentionally small
---

This fixture exists only to prove that a repo workflow pack can load a valid
workspace skill.
