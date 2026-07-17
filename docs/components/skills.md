# Skills

Skills are git-pinned trusted instruction packs that inject context and declare
tool routing.

## What Exists

Core skills ship with the binary and can be used immediately:

```bash
agl skill list --source core
agl skill inspect repo-status --runtime
agl run --skill repo-status --prompt "Summarize this repo status."
```

Workspace skills may live under an explicitly configured `.agl/skills` Git
artifact and must pass the workspace trust flow before the runtime can inject
them. Core skills do not require this artifact.

```bash
agl skill init
agl skill status
agl skill lock
agl skill trust <name> --yes
agl skill verify
agl skill list --trusted-only
```

Use `agl skill inspect <name> --runtime` to answer "can this skill be used by
run/chat right now?".

## Adding Or Editing Skills

Add or edit a workspace skill by changing the corresponding `SKILL.md` under
`.agl/skills`. After any meaningful change, refresh the lock and trust record:

```bash
agl skill status
agl skill lock
agl skill trust <name> --yes
agl skill verify
```

The lock records the workspace skill Git identity in `.agl/skills.lock`.
The local trust decision is stored under the agentLIBRE state directory:

```bash
agl config paths
```

Look for `state_dir`, then inspect `skill-trust.toml` below that directory.

## Runtime Visibility

`agl run` loads selected skills when the command starts. Run the command again
after changing a selected skill.

Bare `agl` opens the daemon-backed interactive session. Selected skill context
and visible tools can be refreshed from Command mode with:

```text
/reload
```

Use `/status` to print the current session and workspace state. Use
`/workspace PATH` to change the filesystem tool root; this also refreshes
runtime skill/tool context.

Changing the daemon inference profile, model paths, runtime model settings, or
`[prompt].skills` requires starting a new interactive session or running a new
`agl run`.

## Evidence And Logs

Use these commands first:

```bash
agl config status
agl config paths
```

Important paths:

- `app_log`: application logs.
- `inference_log`: inference/backend logs.
- `sessions_root`: persisted chat transcripts.
- `data_dir/runs` or the chat `artifact_root`: run artifacts and evidence.
- `<artifact_root>/<run_id>/skill-context.json`: selected skill context evidence.
- `<artifact_root>/<run_id>/skill-folder-runtime-prepare.json`: skill folder preparation evidence when folders are created or errors occur.

If a skill is not visible at runtime, check in this order:

```bash
agl skill inspect <name> --runtime
agl skill status --strict
agl skill verify
agl config status --strict
```
