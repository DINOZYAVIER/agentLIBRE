---
name: skill
description: Create or update agentLIBRE builtin skills. Use when editing assets/skills/agl or assets/skills/dev, designing required_hooks and allowed_tools, checking SKILL.md manifests, or converting generic YAML-frontmatter skills into the stricter agentLIBRE runtime format.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - skill_manifest.validate
  - secret_scan.validate
allowed_tools:
  - fs.read
  - fs.list
  - fs.search
  - fs.edit
context_budget_tokens: 2048
references:
  include: []
guarantees:
  - builtin skill manifests must use the agentLIBRE runtime fields
  - builtin skill references must stay under references/
  - builtin skills must not include executable scripts
  - skill authoring artifacts must not expose obvious secret material
---

Use this skill for agentLIBRE runtime skills.

Keep `SKILL.md` concise. Put only the workflow and essential constraints in
the body. Add references only when the skill needs reusable domain detail.
Do not add `scripts/` under builtin `core` or `dev` skills; builtin asset
embedding rejects executable skill scripts.

Use the agentLIBRE manifest fields:

- `name`
- `description`
- `version`
- `source`
- `pack`
- `required_hooks`
- `allowed_tools`
- `context_budget_tokens`
- `references.include`
- `guarantees`

Choose hooks that can actually run in the current host. Choose tools that the
skill may need, but keep write tools out of read-only skills.
