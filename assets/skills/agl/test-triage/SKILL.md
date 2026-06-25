---
name: test-triage
description: Investigate failing tests and verification output in agentLIBRE. Use when tests, cargo checks, smoke tests, or CI-style commands fail and the agent must classify the failure, find the responsible code path, and avoid blind edits.
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
  - triage summaries must include observed verification evidence
  - triage must distinguish likely regression, environment issue, and stale expectation
  - triage artifacts must not expose obvious secret material
---

Use this skill when verification fails.

Start from the first real failure, not the noisiest follow-on error. Identify
the command, package, test name, and failing assertion or compiler diagnostic.

Read the failing test and nearby implementation before proposing a fix.
Classify the issue as one of:

- real behavior regression
- stale or incomplete test expectation
- environment or missing dependency
- flaky timing or external-state issue

Do not edit production code just to satisfy a test until the intended behavior
is clear. In the final answer, include the evidence and the recommended next
change or the fix already made.
