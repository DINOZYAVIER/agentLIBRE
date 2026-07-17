---
schema: agentfunction/v1
id: gemma4-e4b
title: Gemma 4 E4B
description: Conservative local Gemma 4 E4B QAT function with vision and native tool calls.
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
