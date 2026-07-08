Advanced repo workspace commands.

Repo workspace:
- .agl/workspace.toml lists the .agl folders for this repo.
- components list paths such as .agl/skills and .agl/tasks.
- artifact_sources list the .agl folders agl is allowed to manage.
- profiles can be exported, edited, checked with --dry-run, then applied.

Typical workflow:
  agl repo status
  agl repo export-profile --out profile.toml
  agl repo init --profile-file profile.toml --dry-run
  agl repo artifact verify
