Release resident inference resources through the running agentLIBRE daemon.

Use exactly one target:

- `--all` releases every resident context and model.
- `--digest DIGEST` releases one model selected by its 64-character lowercase
  SHA-256 digest.

An active matching model is reported as busy and is never cancelled. A target
that is already nonresident succeeds idempotently.
