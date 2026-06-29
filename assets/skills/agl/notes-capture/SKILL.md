---
name: notes-capture
description: Identify useful agentLIBRE notes. Use when work produces a plan, research summary, bug reproduction, handoff, review summary, or temporary project context that should be kept as a note before optional memory promotion.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - secret_scan.validate
allowed_tools:
  - fs.read
  - fs.search
  - notes.add
  - notes.search
  - notes.show
  - notes.link
denied_tools:
  - notes.delete
  - notes.remember
  - notes.update
permissions:
  notes:
    read: true
    write: true
context_budget_tokens: 1536
references:
  include: []
guarantees:
  - notes capture must be explicit and reviewable
  - note candidates must include title and body
  - note candidates must not include secrets or private local state
---

Use this skill to create explicit notes through `notes.add` when the user asks
to keep project context, handoffs, research summaries, or temporary workflow
state. Do not create a note silently; the note must be an intentional artifact
of the current task.

Notes are useful for handoffs, temporary analysis, longer research, and
project-specific context that may or may not become memory later. Keep note
bodies readable and organized enough that a future agent can use them without
reconstructing the whole conversation.

Use `notes.search` and `notes.show` to avoid duplicates before creating a note
when the topic is likely already recorded. Use `notes.link` when a created note
should reference a task, review, memory, Matrix room, or repository artifact.

When a note deserves long-term retrieval, suggest explicit promotion to memory
as a separate decision.
