# AGENTS.md

## Repository map

| Path | Responsibility |
| --- | --- |
| `README.md` | Short project description. |
| `Cargo.toml`, `Cargo.lock` | Crate manifest and locked dependencies. |
| `src/` | The `agl` and `agentLIBRE` binaries (one `main.rs`) of the `agentLIBRE` crate: the command-line surfaces (`cli/`: arguments, the stdin line protocol, `-p` progress output, `agl server` and its systemd unit, `agl init`, the `agl forge` store), the artifact store engine (`forge/`: the `forge/v1` envelope, the project identity and its `agentLIBRE.toml`, found among the registered projects from the cwd up, the `task-spec` type), XDG paths (`paths.rs`), the user `agentLIBRE.toml` (`settings.rs`), strict TOML reading (`tomlfile.rs`), errors (`error.rs`) and their English/Ukrainian messages (`i18n.rs`), grammars and GBNF (`grammar/`), model formats (`format.rs`), Functions (`function/`), built-in formats and Functions (`builtin/`), the turn state machine (`machine/`), action grammars and output parsing (`action/`), the inference backend and its llama-server client (`backend/`), the bash, edit and view tools (`tools/`), session files (`session/`), the media of a message — images, video, audio, their placeholders and files (`media.rs`) — and what the human attaches and the transcription of an audio (`attach.rs`, `cli/whisper.rs`), the agent turn (`agent/`), the vocabulary shared by the interactive and line sessions (`status.rs`), Ctrl-C handling (`interrupt.rs`), terminal colors (`theme.rs`), the interactive session and its terminal (`repl/`: line editor, dock, Markdown, text wrapping, transcript, terminal and its keys). |
| `LICENSE.md`, `LICENSES/` | License summary and official license texts. |
| `~/.local/share/agentLIBRE/agl-lite-<sha256-12>/forge/` | The workspace forge in XDG_DATA_HOME: the AGL-XXX task specs (`task-spec` artifacts) and the other artifact types, one store per git toplevel, managed by `agl forge`. |
| `../agl-lite-tasks/` | The non-artifact working documents, archived outside the repository (not tracked): MEMORY_SPEC.md, the feedback notes, the README. |

Update this map when adding top-level paths.

## Alpha compatibility policy

agentLIBRE is in alpha. Do not add backward-compatibility paths, legacy
fallbacks, migration shims, or compatibility-preserving behavior unless the
human explicitly asks for them in the current task.

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
- Remote: private GitHub repository `DINOZYAVIER/agl-lite`. Push only when
  the human asks.

## LLM-assisted commits

LLM agents may prepare patches and draft commit messages, but they are tools,
not legal contributors.

A human must review, understand, and take responsibility for the final commit.
