# AGENTS.md

## Repository map

| Path | Responsibility |
| --- | --- |
| `README.md` | Short project description. |
| `Cargo.toml`, `Cargo.lock` | Crate manifest and locked dependencies. |
| `flake.nix`, `flake.lock` | The Nix flake (AGL-040): the package (both binaries behind a wrapper that sets `AGENTLIBRE_TOOLS` and `AGENTLIBRE_NIX`), the directory of the helper programs, the dev shell, the checks, `lib` (the `agl` argument); `x86_64-linux`. |
| `nix/` | The built-ins in Nix (AGL-039): the `agl` argument of every Function and settings file (`default.nix`) — the policies, the formats, the models and the built-in Functions. |
| `src/` | The `agl` and `agentLIBRE` binaries (one `main.rs`) of the `agentLIBRE` crate: the command-line surfaces (`cli/`: arguments, the stdin line protocol, `-p` progress output, `agl server` and its systemd unit, `agl init`, the `agl forge` store, the Functions of `agl function`, the client certificate of `agl search login`, the lines on stdout), the artifact store engine (`forge/`: the `forge/v2` envelope (a Nix front matter), the project identity in its `settings.nix`, found among the registered projects from the cwd up, the `task-spec` type), XDG paths (`paths.rs`), the helper programs agl runs by their absolute paths in `AGENTLIBRE_TOOLS` (`programs.rs`), the `settings.nix` of the user and of the project (`settings/`: the kind, the files and the assembly, and a module per service section — `server`, `whisper`, `search`, `repl`), the evaluation of Nix by `nix-instantiate`, the writing of Nix and the strict reading of evaluated values (`nix/`), the weights folders (`weights.rs`), errors (`error.rs`) and their English/Ukrainian messages (`i18n.rs`), grammars and GBNF (`grammar/`), model formats (`format.rs`), Functions (`function/`), built-in formats and Functions (`builtin/`), the turn state machine (`machine/`), the FSM during a turn and what it writes to the session file (`phase.rs`), action grammars and output parsing (`action/`), the inference backend and its llama-server and Strata clients (`backend/`), the bash, edit and view tools (`tools/`), the search tool, the human's search service over mutual TLS by `curl` (`search/`), session files (`session/`), the media of a message — images, video, audio, their placeholders and files (`media.rs`) — and what the human attaches and the transcription of an audio (`attach.rs`, `cli/whisper.rs`), the recording of the human's voice (`record.rs`), the agent turn (`agent/`), the workspace memory, its entries and its index (`memory.rs`), the vocabulary shared by the interactive and line sessions (`status.rs`), Ctrl-C handling (`interrupt.rs`), the helpers of the ignored tests against a live server (`test_server.rs`), terminal colors (`theme.rs`), the interactive session and its terminal (`repl/`: line editor, dock, Markdown, text wrapping, transcript, terminal and its keys). |
| `LICENSE.md`, `LICENSES/` | License summary and official license texts. |
| `~/.config/agentLIBRE/` | The user's `settings.nix` and `functions/*.nix` (AGL-039). |
| `~/.local/share/agentLIBRE/agl-lite-<sha256-12>/` | The workspace store: its `settings.nix` (the identity and the project's settings, AGL-041), `functions/*.nix`, `memory/`, `forge/`, the JSON record `server.state` of the server agl launched. |
| `~/.local/share/agentLIBRE/agl-lite-<sha256-12>/forge/` | The workspace forge in XDG_DATA_HOME: the AGL-XXX task specs (`task-spec` artifacts) and the other artifact types, one store per git toplevel, managed by `agl forge`. |
| `../agl-lite-tasks/` | The non-artifact working documents, archived outside the repository (not tracked): MEMORY_SPEC.md, the feedback notes, the README, `scripts/forge-v2-migrate.py` (the one-off rewrite of AGL-041). |

Update this map when adding top-level paths.

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
- Remote: private GitHub repository `DINOZYAVIER/agentLIBRE`. Push only when
  the human asks.

## LLM-assisted commits

LLM agents may prepare patches and draft commit messages, but they are tools,
not legal contributors.

A human must review, understand, and take responsibility for the final commit.
