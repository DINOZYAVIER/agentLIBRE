---
name: commit-hygiene
description: Check agentLIBRE commit hygiene. Use when preparing task-sized commits, reviewing commit boundaries, validating LLM-assistance trailers, or checking that a wave is split into sensible commits.
version: 1
source: builtin
pack: agl
required_hooks:
  - commit_message.validate
  - secret_scan.validate
allowed_tools:
  - fs.read
  - fs.search
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - LLM-assisted commits must not add LLM Signed-off-by or Co-authored-by trailers
  - LLM-assisted patches must disclose assistance with an Assisted-by trailer
  - each commit should map to one task or one coherent slice
---

Use this skill for commit boundaries and messages.

Follow repository `AGENTS.md` before any generic convention. Keep commits
small enough to review and revert. Prefer one task or one independently useful
slice per commit, especially when a wave touches store, CLI, assets, and repo
workflow code.

Commit messages should state the task and behavior change. Do not include DCO
trailers unless a human certifies them.
