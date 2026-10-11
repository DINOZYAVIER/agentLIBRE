# AGENTS.md

## Repository map

| Path | Responsibility |
| --- | --- |
| `README.md` | Short project description. |
| `SPEC.md` | Working spec and human decisions (Ukrainian). Read it before any task. |
| `Cargo.toml`, `Cargo.lock` | Crate manifest and locked dependencies. |
| `src/` | The `agl-lite` binary: arguments (`cli.rs`), XDG paths (`paths.rs`), `agentLIBRE.toml` files (`settings.rs`), errors (`error.rs`) and their English/Ukrainian messages (`i18n.rs`), grammars and GBNF (`grammar.rs`, `grammar/`). |
| `experiments/` | Grammar probes and raw results behind `SPEC.md` Appendix B; not part of the product. |
| `LICENSE.md`, `LICENSES/` | License summary and official license texts. |

Update this map when adding top-level paths.

## Alpha compatibility policy

agl-lite is in alpha. Do not add backward-compatibility paths, legacy
fallbacks, migration shims, or compatibility-preserving behavior unless the
human explicitly asks for them in the current task.

## Human decision gate

The human is the sole authority for project decisions. A recommendation, an
inferred preference, or an LLM's engineering judgment is not a decision.

- Record decisions in `SPEC.md`. Do not ask again for choices already
  recorded there.
- Do not implement a material product, architecture, naming, API, workflow,
  scope, security, data-format, or compatibility choice while it is open.
- Do not start implementation until the human states that the spec for that
  task is complete.

## Engineering output discipline

Every normative statement in the spec or a review must be traceable to current
code, tests, or configuration; an accepted decision in `SPEC.md`; or an
explicit human decision in the current work. Otherwise state it as an open
question. Never turn an inference into a requirement, default, or invariant.

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

Only a human may certify the DCO with `Signed-off-by`. LLM agents must not add
`Signed-off-by` or `Co-authored-by` for themselves.

If an LLM or advanced coding tool meaningfully helped create the patch,
disclose it with:

```text
Assisted-by: AGENT_NAME:MODEL_VERSION [TOOL...]
```

For example: `Assisted-by: Claude-Code:claude-opus-5-5`.

List tools only when they materially found, generated, transformed, or
validated the patch. Do not list ordinary development tools such as `git`,
editors, build or test commands, formatters (`cargo fmt`), autocomplete, or
mechanical renames. Mention `cargo clippy` only when a specific finding
materially shaped the patch.

A human must review, understand, and take responsibility for the final commit.
