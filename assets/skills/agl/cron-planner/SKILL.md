---
name: cron-planner
description: Plan trusted agentLIBRE cron jobs. Use when turning a recurring workflow into a safe schedule, choosing human or cron syntax, selecting builtin or trusted skill targets, and deciding Matrix notification behavior.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - verification.validate
  - secret_scan.validate
allowed_tools:
  - fs.read
  - fs.search
  - cron.preflight
requestable_tools:
  - cron.add
  - matrix.outbox.enqueue
denied_tools:
  - matrix.outbox.deliver
permission_request_templates:
  - id: schedule-matrix-cron
    tools:
      - cron.add
      - matrix.outbox.enqueue
    max_operation_kind: write
    default_duration: one_turn
    reason_template: Schedule a recurring Matrix notification.
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - cron plans must not use arbitrary shell targets
  - cron plans must name schedule, target kind, target id, and notification policy
  - cron plans must call out trust requirements for skill targets
---

Use this skill when designing recurring AgentLIBRE work.

Keep cron jobs narrow and idempotent. Prefer builtin targets for diagnostics
and trusted skills for repository workflows. Do not suggest arbitrary shell
commands as cron targets.

For each proposed job, include the schedule, target, expected result, failure
handling, and whether Matrix notification should be enabled.
