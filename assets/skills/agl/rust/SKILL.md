---
name: rust
description: Work in the agentLIBRE Rust workspace. Use for Rust crate edits, module splits, Cargo workspace changes, test failures, trait/API adjustments, and verification choices around cargo fmt, cargo test, cargo check, and cargo clippy.
version: 1
source: builtin
pack: agl
required_hooks:
  - repo_path.validate
  - diff_scope.validate
  - secret_scan.validate
  - verification.validate
allowed_tools:
  - fs.read
  - fs.list
  - fs.search
  - fs.edit
context_budget_tokens: 2048
references:
  include: []
guarantees:
  - Rust edits must preserve crate boundaries unless the task requires changing them
  - final summaries must identify cargo verification or a clear not-run reason
  - Rust workspace artifacts must not include generated junk or obvious secrets
---

Use this skill for Rust workspace changes.

Read the crate's public exports, nearby tests, and existing error style before
changing APIs. Keep root `lib.rs` files small when the crate already uses
modules, but preserve public re-exports that callers rely on.

Prefer targeted verification:

- run formatting after Rust edits
- run package tests for touched crates
- run workspace checks when public contracts, shared crates, or generated
  assets changed

Do not add dependencies until an existing crate or standard library approach is
insufficient. If a dependency is needed, explain why in the final answer.

In the final answer, include changed crates and exact verification.
