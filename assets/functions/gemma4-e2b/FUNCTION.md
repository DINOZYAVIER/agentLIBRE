---
schema: agentfunction/v1
id: gemma4-e2b
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
