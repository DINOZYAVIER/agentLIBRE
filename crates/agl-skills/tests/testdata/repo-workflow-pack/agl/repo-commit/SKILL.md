---
name: repo-commit
description: Draft or check agentLIBRE commit messages and checkpoint history. Use when preparing commits, reviewing recent commits, checking trailers, applying AGENTS.md LLM-assistance rules, or reasoning about tags, version bumps, and checkpoint boundaries.
version: 1
source: workspace
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
  - commit artifacts must not expose obvious secret material
---

Use this skill for commit messages and history hygiene.

Follow repository `AGENTS.md` first. LLM agents must not certify DCO or add
themselves as co-authors. Use `Assisted-by: Codex:gpt-5` when this agent
meaningfully helped create a patch. List specialized tools only when they
materially found, generated, transformed, or validated the patch.

Keep subject lines imperative and scoped. Keep the body focused on why the
change exists and what behavior changed. Do not include test logs unless a
short verification line is enough.

For checkpoint work, treat signed tags as version boundaries and keep human
`Signed-off-by` trailers for human-owned checkpoint/version bump commits.
