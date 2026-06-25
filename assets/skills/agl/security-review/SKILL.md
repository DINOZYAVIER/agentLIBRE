---
name: security-review
description: Check agentLIBRE changes for accidental sensitive data and unsafe diff scope. Use when reviewing patches, generated artifacts, config examples, Matrix bridge changes, credentials handling, environment variables, or any change likely to expose tokens, private paths, or local state.
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
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - security review artifacts must not expose obvious secret material
  - security review must call out generated junk and local-only paths when present
  - final summaries must include what was checked
---

Use this skill for security-oriented patch review.

Scan for credentials, access tokens, private keys, local absolute paths,
session dumps, and generated directories. Treat placeholder values as allowed
only when they are clearly examples or tests.

Pay special attention to Matrix bridge state, environment examples, config
loading, logs, transcripts, review artifacts, and generated caches.

In the final answer, state what was scanned, what was found, and whether any
remaining risk needs human review.
