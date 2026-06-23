---
name: task-spec
description: Write and validate agentLIBRE task specs.
version: 1
source: builtin
pack: core
required_hooks:
  - repo_path.validate
  - task_spec.validate
allowed_tools:
  - file_read
  - file_write
context_budget_tokens: 4096
references:
  include:
    - references/task-spec-contract.md
guarantees:
  - generated task specs must pass task_spec.validate
  - referenced repository paths must pass repo_path.validate
---

Use this skill when preparing or revising an agentLIBRE task spec.

Keep the task scoped, implementation-oriented, and tied to verifiable
acceptance criteria. Prefer explicit non-goals over broad promises.
