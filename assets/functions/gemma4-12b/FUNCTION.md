---
schema: agentfunction/v1
id: gemma4-12b
title: Gemma4 12B
description: Local Gemma4 12B QAT agent function with native Gemma tool-call formatting.
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
  smoke_prompt: "Reply with function=gemma4-12b and summarize the visible runtime identity."
contracts:
  identity:
    mode: validate_claims
    fields:
      - function
      - skills
      - subagents
    repair: true
    max_repair_attempts: 1
---
