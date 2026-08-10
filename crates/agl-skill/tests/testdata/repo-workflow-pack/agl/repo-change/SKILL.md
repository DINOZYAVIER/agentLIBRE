---
package:
  schema: agentlibre.package/v1
  type: skill
  id: repo-change
  version: 1.0.0
  payload_schema: agentlibre.skill/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires: []
description: Synthetic workspace skill used to validate repo workflow pack parsing.
pack: agl
required_hooks:
  - core:repo_path.validate
allowed_tools: []
context_budget_tokens: 128
references:
  include: []
guarantees:
  - fixture skill stays intentionally small
---

This fixture exists only to prove that a repo workflow pack can load a valid
workspace skill.
