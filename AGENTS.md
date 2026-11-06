# AGENTS.md

## Compatibility policy

Do not add backward-compatibility paths, legacy fallbacks, migration shims, or
compatibility-preserving behavior unless the human explicitly asks for them in
the current task.

## Human decision gate

The human is the sole authority for project decisions. A recommendation, an
inferred preference, or an LLM's engineering judgment is not a decision.

- Record decisions in the AGL-XXX task-spec of the task, in the
  workspace forge (the XDG data directory, `agl forge`). The
  task-specs are the reference for decisions; the workspace memory
  (`agl`'s `memory` tool and `/remember`, AGL-006) holds facts about
  the project, its environment and the human's preferences, not
  decisions. Do not ask again for choices recorded there.
- Do not implement a material product, architecture, naming, API, workflow,
  scope, security, data-format, or compatibility choice while it is open.
- Do not start implementation until the human states that the spec for that
  task is complete.

## Engineering output discipline

Every normative statement in the spec or a review must be traceable to current
code, tests, or configuration; an accepted decision in an AGL-XXX task-spec
of the workspace forge; or an explicit human decision in the current work.
Otherwise state it as an open question. Never turn an inference into a
requirement, default, or invariant.

Ask only questions whose answers change code, stored data, security, or
observable behavior, and state them with concrete files, types and
consequences. Keep output concise and exact.

## Workflow, commits and tags

- Work proceeds task by task. Each task is a short series of small commits.
- When a task is ready for review, create an annotated tag
  `task-NN-<slug>` (for example `task-01-llama-client`) on its last commit.
  Tags mark tasks, not versions. Review fixes go in new commits; re-tag only
  when the human asks.
- Remote: public GitHub repository `DINOZYAVIER/agentLIBRE`. Push only when
  the human asks.

## LLM-assisted commits

LLM agents may prepare patches and draft commit messages, but they are tools,
not legal contributors.

A human must review, understand, and take responsibility for the final commit.
