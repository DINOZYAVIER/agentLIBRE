# AGENTS.md

Each directory contains `EXTENSION.toml` and JSON Tool/Effect schemas.

- `agentlibre-files/`: `file.read` and `file.patch` Tools; `file.write` Effect and its scope.
- `agentlibre-forge/`: `artifact.read` for declared artifacts.
- `agentlibre-execution/`: command execution, terminal sessions and execution scope.
- `agentlibre-searxng/`: search/query schemas and `fixtures/v1.json`.

Native handlers live in `crates/agl-tools/`. The root manifest pins Extension
sources/revisions; its SearxNG declaration currently points to a separate
repository. There is no `builtins` Extension directory.
