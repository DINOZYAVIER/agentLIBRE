---
name: repo-review-pack
description: Prepare agentLIBRE HTML code review packs. Use when asked to create or update review artifacts under .agl/reviews, especially for a commit, branch range, patch, or uncommitted change set that must follow the manifest, payload, render, index, and validate pipeline.
version: 1
source: workspace
pack: agl
required_hooks:
  - repo_path.validate
  - review_pack.validate
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
  - review artifacts must be written under .agl/reviews
  - review output must include manifest, payload, rendered HTML pages, and an index
  - final summaries must include review validation evidence
---

Use the repository review-pack workflow, not hand-written standalone HTML.

Prepare a review manifest that points at the target repository, branch, base,
and head. Build payload data from the manifest, render PR/file/diff/
implementation pages, update the index, then validate the generated pack.

When reviewing uncommitted work, use a temporary commit object only if needed
to feed the review pipeline. Do not move the user's branch or change the real
index unless explicitly asked.

In the final answer, include the `.agl/reviews/.../index.html` path and the
review validation result.
