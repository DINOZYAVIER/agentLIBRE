# AGENTS.md

## Checking work

- Before a commit: `nix flake check`: the build, the tests, clippy, the Functions and
  the NixOS module.
- One test: `nix develop --command cargo test <name>`: the dev shell sets the variables
  the tests need.
- Tests marked `#[ignore]` need a live server:
  `AGENTLIBRE_TEST_SERVER=http://127.0.0.1:18080 cargo test -- --ignored`.

## Outside the repository

- `$XDG_DATA_HOME/agentLIBRE/<project>-<sha256-12>/` (`~/.local/share` by default): the
  store of a project: its `settings.nix`, its memory, and the AGL-XXX task specs in
  `forge/task-spec/`, managed by `agl forge`.
- `$XDG_CONFIG_HOME/agentLIBRE/` (`~/.config` by default): the user's `settings.nix` and
  `functions/*.nix`.
- `github:DINOZYAVIER/agentLIBRE-functions`: the Functions agl ships, pinned in
  `flake.lock`.

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
consequences.

## Git

- `develop` takes the work; `master` takes `develop` only at the human's word.
- A task is a short series of small commits, one concern each.
- Conventional Commits with the AGL-XXX of the task: `fix(memory): ... (AGL-058)`.
  The body says why, when the subject does not.
- No Co-Authored-By, Signed-off-by or other attribution of an agent, in a commit or a
  pull request.
- Remote: public GitHub repository `DINOZYAVIER/agentLIBRE`. Push only when the human asks.

## LLM-assisted commits

LLM agents may prepare patches and draft commit messages, but they are tools,
not legal contributors.

A human must review, understand, and take responsibility for the final commit.
