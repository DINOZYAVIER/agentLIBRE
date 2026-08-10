Advanced repository commands.

Repository layout:
- .agl/workspace.toml contains only package sources, policy, config, and the default function.
- Runtime Artifacts are named Git submodules; their section name is the exact ArtifactId.
- Git adds and removes Artifact submodules. AGL only verifies bindings and performs admitted operations.

Typical workflow:
  agl repo init
  agl artifact verify
  agl repo verify-tasks
