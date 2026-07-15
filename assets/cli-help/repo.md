Advanced repo workspace commands.

Repo workspace:
- .agl/workspace.toml lists the .agl folders for this repo.
- each opt-in root is declared once under `[artifacts.<id>]`.
- undeclared roots are not created, inspected, or reported as missing.
- profiles can be exported, edited, checked with --dry-run, then applied.

Typical workflow:
  agl repo status
  agl repo export-profile --out profile.toml
  agl repo init --profile-file profile.toml --dry-run
  agl repo artifact verify
