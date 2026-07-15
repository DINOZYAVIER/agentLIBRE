# Artifacts

Artifacts are opt-in workspace data roots under `.agl`. The workspace manifest
is their only source of configuration, and each root appears once:

```toml
[artifacts.tasks]
kind = "git"
path = ".agl/tasks"
url = "ssh://git@example.invalid/agentlibre/specs.git"
rev = "main"
required = true
access = "read_write"
validation = "agl.task_spec.v1"
```

An undeclared root is not created, inspected, or reported as missing. A
declared required root must exist and pass its configured validation. Private
Git roots are independent local checkouts, not public repository submodules.
