---
artifact:
  schema: agentlibre.artifact/v1
  type: function
  id: gemma4-e2b
  version: 1.0.0
  payload_schema: agentlibre.function/v2
  agl:
    compatible: ">=1.0.0-alpha.12"
    tested: [1.0.0-alpha.12]
  requires:
    - model:gemma4-e2b@^1.0
    - extension:core.workspace@^1.0
    - extension:core.process@^1.0
title: Gemma 4 E2B
description: Local Gemma 4 E2B official QAT function with native tool calls.
model:
  config: inference.toml
runtime:
  tool_mode: read-only
  max_output_tokens: 256
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
