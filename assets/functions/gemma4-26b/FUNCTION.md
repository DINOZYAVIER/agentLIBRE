---
schema: agentfunction/v1
id: gemma4-26b
title: Gemma4 26B-A4B
description: Local Gemma4 26B-A4B QAT agent function with native Gemma tool-call formatting.
model:
  config: inference.toml
runtime:
  tool_mode: read-only
  max_output_tokens: 512
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Reply with function=gemma4-26b and summarize the visible runtime identity."
validation:
  runtime_identity:
    required: false
    fields:
      - function
      - skills
      - subagents
    repair_attempts: 1
---
