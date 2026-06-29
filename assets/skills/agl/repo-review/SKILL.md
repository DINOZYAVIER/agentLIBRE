---
name: repo-review
description: Review repository changes in agentLIBRE. Use for code-review style findings, architecture risks, regression checks, missing tests, unsafe diff scope, or deciding whether a patch is ready to merge.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - diff_scope.validate
  - secret_scan.validate
  - verification.validate
allowed_tools:
  - fs.read
  - fs.list
  - fs.search
context_budget_tokens: 2048
references:
  include: []
guarantees:
  - review findings must be grounded in repository paths or observed behavior
  - review output must prioritize bugs, regressions, security risks, and missing tests
  - review artifacts must not expose obvious secret material
---

Use this skill for review-first repository work.

Start from the changed surface, then read the contracts and tests that define
expected behavior. Lead with concrete findings, ordered by severity. Include
file paths, symbols, commands, or observed output when they are the basis for a
finding.

Do not turn a review into a broad rewrite proposal. If no issue is found, say
that clearly and call out remaining test gaps or residual risk.
