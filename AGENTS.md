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
