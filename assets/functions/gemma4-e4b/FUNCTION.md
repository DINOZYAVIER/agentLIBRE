---
package:
  schema: agentlibre.package/v1
  type: function
  id: gemma4-e4b
  version: 1.2.0
  payload_schema: agentlibre.function/v3
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - model:gemma4-e4b@^1.0
    - extension:core.workspace@^1.0
    - extension:core.process@^1.0
title: Gemma 4 E4B
description: Conservative local Gemma 4 E4B QAT function with vision and native tool calls.
model:
  profile: gpu-rx7900xtx-32768
runtime:
  tool_mode: read-only
  max_output_tokens: 256
  stop_rules: []
  structured_generation: lazy_tool
  repair_malformed_tool_calls: true
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Reply with exactly: agentLIBRE ready"
validation:
  runtime_identity:
    required: false
    fields:
      - function
      - skills
      - subagents
    repair_attempts: 1
---
