# Skills

A Skill is a bounded immutable entity parsed from
`skills/<ui-name>/SKILL.md`. Its direct `agentlibre.skill/v1` front matter
contains identity, optional description, required Tool IDs and contained
reference paths; the Markdown body contains instructions.

Function activation resolves every locked Skill, verifies each declared
reference file, and materializes its instructions into the frozen
`AgentRunSnapshot`.

Skills do not grant authority, install handlers, carry trust or permission
state, own budgets, or have lifecycle/FSM state.
