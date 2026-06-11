# AGENTS.md

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

Examples:

```text
Assisted-by: Codex:gpt-5.5
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
