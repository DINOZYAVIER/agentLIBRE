# AGENTS.md

## Alpha compatibility policy

agentLIBRE is in alpha. Do not add backward-compatibility paths, legacy
fallbacks, migration shims, or compatibility-preserving behavior unless the user
explicitly asks for them in the current task. Prefer one clear breaking format
over extra code that keeps obsolete formats alive.

## Default agentLIBRE filesystem layout (XDG)

The paths below are the current defaults. They apply when `AGL_HOME` is unset
and the corresponding XDG variable is unset; an XDG variable replaces only
its own base directory. These locations may become configurable later.

The application directory is `agentLIBRE` below each XDG base:

| Root | Default base | Current contents |
| --- | --- | --- |
| Config | `${XDG_CONFIG_HOME:-$HOME/.config}/agentLIBRE` | Human config: `agentLIBRE.toml`; generated systemd user units are in `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/`. The Matrix service script defaults to `matrix-bridge/agl.toml`. |
| Data | `${XDG_DATA_HOME:-$HOME/.local/share}/agentLIBRE` | Installed source tree: `source/` (including `source/functions/`); generated active config: `config/active.json`; daemon store: `store/agentlibre.sqlite3` with SQLite `-wal`/`-shm` sidecars; Function/package cache: `runtime/packages/`; managed GGUF models: `runtime/models/<digest>.gguf`; execution store: `execd/executions.sqlite3`. |
| State | `${XDG_STATE_HOME:-$HOME/.local/state}/agentLIBRE` | Daemon socket: `daemon/agl.sock`; execution socket: `execd/execd.sock`. |
| Cache | `XDG_CACHE_HOME` | Not currently used by the production agentLIBRE code. Do not assume an agentLIBRE cache directory here. |

Function resolution uses `source/functions/` from the installed source tree by
default, plus any directories listed in `[packages].directories` in
`agentLIBRE.toml`. `FUNCTION.toml` and `FUNCTION.lock` remain next to each
Function source directory. The Matrix bridge's session, encrypted-room store,
and binding state are explicit paths in its TOML config; they are not inferred
from XDG at runtime. The repository example places them below
`$XDG_DATA_HOME/agentLIBRE/matrix-bridge/`.

Service logs are not written to an agentLIBRE file by the systemd units. Both
stdout and stderr go to the per-user systemd journal:

```text
journalctl --user -u agentlibre-execd.service
journalctl --user -u agentlibre-daemon.service
journalctl --user -u agl-matrix-bridge.service
```

For an isolated CLI or `agl-execd` invocation, `AGL_HOME=/absolute/path`
replaces the three application roots with `$AGL_HOME/config`,
`$AGL_HOME/data`, and `$AGL_HOME/state`; this is an explicit override, not the
default layout above.

## LLM-assisted commits

LLM agents may prepare patches and draft commit messages, but they are tools,
not legal contributors.

Only a human may certify the DCO with `Signed-off-by`. LLM agents must not add
`Signed-off-by` or `Co-authored-by` for themselves.

If an LLM or advanced coding tool meaningfully helped create the patch, disclose
it with:

```text
Assisted-by: AGENT_NAME:MODEL_VERSION [TOOL...]
```

Examples are illustrative, not prescribed. Use the actual tool and model that
materially assisted the patch; it may be Codex, Opencode, Claude Code, or any
other coding agent/model.

```text
Assisted-by: Codex:gpt-5.5
Assisted-by: Opencode:MODEL_VERSION
Assisted-by: Claude-Code:MODEL_VERSION
Assisted-by: Codex:gpt-5.5 coccinelle sparse
```

List tools only when they materially found, generated, transformed, or
validated the patch. Do not list ordinary development tools or mechanical
helpers such as `git`, editors, build commands, test commands, formatters,
ordinary autocomplete, spelling/grammar fixes, or mechanical renames.

`cargo fmt`/`rustfmt` are formatters and should not be listed. `cargo clippy`
is a lint/static-analysis tool; mention it only when a specific finding or fix
materially shaped the patch, not when it was merely run as a routine check.

A human must review, understand, and take responsibility for the final commit
before submission.

## Human decision gate

For task planning and implementation, use the human decision gate below. A
recommendation, an inferred preference, or an LLM's engineering judgment is
not a human decision.

Do not implement a material product, architecture, naming, API, workflow,
scope, security, data-ownership, or compatibility choice while it remains open.
Collect related open choices in the task's existing decision document, record
the exported human result in the spec, and do not ask again for choices already
recorded there.

## Engineering output discipline

The human is the sole authority for project decisions. The agent audits code,
finds the decisions required to implement the task, explains their concrete
consequences, and implements only the recorded human choice.

Every normative statement in a task spec or review must be traceable to one of:

- current code, tests, or repository configuration;
- an existing accepted spec or decision record; or
- an explicit human decision in the current work.

Otherwise state it as an open question. Never turn an LLM inference into a
requirement, non-goal, default, invariant, or claim that behavior stays
unchanged.

Ask only questions whose answers materially change code, public API, stored
data, security, or observable behavior. State each question using concrete
files, types, transitions, and consequences. Do not use project-management,
marketing, legal, or unexplained abstract vocabulary when direct engineering
language is available.

Keep engineering output concise and exact. Remove repetition. Use repository
identifiers where they improve precision, and explain any unavoidable term in
plain language. Before sending or committing a spec, remove unsupported claims,
unnecessary terminology, and speculative design presented as fact.

Do not infer that a spec is complete because every currently listed choice has
been selected. Close a planning or decision gate only after the human explicitly
states that the spec and its decision set are complete.

## Review checkpoints and versions

Git tags are the source of truth for project checkpoint versions. Do not derive
versions from `Signed-off-by` trailers. Feature and work-in-progress commits on
project branches do not require `Signed-off-by`, but checkpoint version bump
commits do.

Use SemVer pre-release tags for accepted alpha checkpoints, starting with
`v1.0.0-alpha.1`. Increment the alpha number for the next accepted checkpoint
before the stable `v1.0.0` baseline.

Prefer signed tags for approved checkpoints when local signing is configured:

```text
git tag -s v1.0.0-alpha.1 -m "v1.0.0-alpha.1"
```

If signing is not available, use an annotated tag and keep the tag as the
version boundary:

```text
git tag -a v1.0.0-alpha.1 -m "v1.0.0-alpha.1"
```

Use `scripts/bump-workspace-version.sh --dry-run` to preview the next
checkpoint version. Run `scripts/bump-workspace-version.sh` after an approved
checkpoint to update the workspace version, update `Cargo.lock`, commit the
version bump with `Signed-off-by`, and create the signed tag.

`Signed-off-by` remains a DCO/attestation trailer for commits. Use
`Reviewed-by` or `Approved-by` trailers to record human review when useful.
