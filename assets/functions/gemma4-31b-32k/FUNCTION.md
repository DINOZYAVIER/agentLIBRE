---
package:
  schema: agentlibre.package/v1
  type: function
  id: gemma4-31b-32k
  version: 1.2.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - model:gemma4-31b@^1.0
    - extension:core.workspace@^1.0
    - extension:core.process@^1.0
title: Gemma4 31B 32K
description: Local Gemma4 31B QAT 32K GPU agent function with native Gemma tool-call formatting.
model:
  config: inference.toml
runtime:
  tool_mode: read-only
  max_output_tokens: 4096
  max_tool_calls: 32
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Reply with function=gemma4-31b-32k and summarize the visible runtime identity."
validation:
  runtime_identity:
    required: false
    fields:
      - function
      - skills
      - subagents
    repair_attempts: 1
---
