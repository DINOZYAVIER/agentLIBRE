---
name: smoke-test
description: Design or run focused agentLIBRE smoke tests. Use after implementing a feature, fixing a bug, wiring a CLI command, or changing runtime behavior that should be exercised end to end.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - verification.validate
  - secret_scan.validate
allowed_tools:
  - fs.read
  - fs.list
  - fs.search
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - smoke tests must touch the user-visible behavior, not only unit-level helpers
  - smoke summaries must include exact commands or a clear reason commands were not run
  - smoke artifacts must not expose obvious secret material
---

Use this skill for practical feature smoke testing.

Pick the smallest realistic path that exercises the feature from the outside:
CLI command, daemon request, Matrix bridge flow, or persisted store state. Try
both a normal path and one failure or misuse case when the feature is safety
sensitive.

When a smoke test fails, preserve the first real failure and classify whether
it is a regression, stale expectation, environment issue, or incomplete feature.
