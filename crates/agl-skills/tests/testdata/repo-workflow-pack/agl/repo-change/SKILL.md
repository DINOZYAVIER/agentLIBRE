---
name: repo-change
description: Make scoped source changes in an agentLIBRE repository. Use for implementation, cleanup, refactors, bug fixes, and small feature work where the agent must inspect existing code, edit files narrowly, run focused verification, and report what changed.
version: 1
source: workspace
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
  - fs.edit
context_budget_tokens: 2048
references:
  include: []
guarantees:
  - repository paths mentioned in artifacts must stay repo-relative
  - final change summaries must include verification evidence or a clear not-run reason
  - source diffs must not include generated junk or local review artifacts
  - artifacts must not expose obvious secret material
---

Use this skill for ordinary repository edits.

Inspect the nearby code before editing. Prefer existing module boundaries,
helpers, naming, tests, and error style. Keep the diff focused on the user
request and avoid unrelated refactors.

Use `fs.search` to locate existing patterns and `fs.read` to inspect the
smallest relevant files. Use `fs.edit` surgically with exact old text.

After edits, verify at the smallest useful scope first. Prefer targeted tests
or checks for the touched crates, then broaden only when shared behavior or
cross-crate contracts changed.

In the final answer, include:

- what changed
- where the important files are
- verification run, or why verification was not run
