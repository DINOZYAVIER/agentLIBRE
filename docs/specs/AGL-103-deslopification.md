# AGL-103 Deslopification Modular Refactor

## Goal

Reduce large catch-all Rust files by moving cohesive code into named modules,
without changing runtime behavior or public CLI contracts.

## Rules

- Prefer mechanical extraction before semantic cleanup.
- Keep public and `pub(crate)` API surfaces stable unless a follow-up spec says otherwise.
- Commit each coherent slice separately.
- Validate each slice with focused tests and clippy; run full workspace checks before closing the wave.

## Agent Findings

- `agl-store/src/lib.rs` mixed schema migrations, connection lifecycle, domain repositories,
  idempotency, matrix outbox, permission grants, export, and tests.
- `agl-cli/src/lib.rs` mixed command dispatch with store command rendering.
- `agl-cli/src/args.rs` mixed CLI model types, clap DTOs, parser mapping, helpers, and tests.
- Follow-up candidates remain in `agl-repo/src/lib.rs`, `agl-chat/src/session.rs`,
  `agl-skills/src/workspace.rs`, `agl-skills/src/manifest.rs`, `agl-cron/src/lib.rs`,
  and `agl-matrix-bridge/src/runtime.rs`.

## Implemented Slices

1. Move store migrations to `agl-store/src/migrations.rs`.
2. Move `agl store ...` command handling to `agl-cli/src/store.rs`.
3. Move CLI command model types to `agl-cli/src/args/model.rs`.

## Next Slices

1. Split `agl-cli/src/args.rs` further into clap DTOs, parser mapping, validation helpers, and tests.
2. Split `agl-store/src/lib.rs` into error/status/domain-specific modules.
3. Split `agl-repo/src/lib.rs` into workspace discovery, component health, hooks, and profile import/export.
4. Split `agl-chat/src/session.rs` into session state, request assembly, and command handling.
5. Split `agl-matrix-bridge/src/runtime.rs` into sync loop, handler setup, outbox delivery, and trust/verification.
