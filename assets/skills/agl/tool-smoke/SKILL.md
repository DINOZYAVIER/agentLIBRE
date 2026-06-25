---
name: tool-smoke
description: Exercise read-only filesystem tool use for local model smoke tests.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools:
  - fs.read
context_budget_tokens: 1024
references:
  include: []
guarantees:
  - live tool smoke answers must pass repo_path.validate
  - live tool smoke is read-only
---

Use this skill only for agentLIBRE local model smoke tests.

Read the requested fixture with `fs.read`, then answer using the observed
marker. Do not edit files, run commands, or call more than the requested
read-only tool.
