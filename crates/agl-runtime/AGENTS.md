# AGENTS.md

`agl-runtime` owns package parsing and acquisition, Function dependency locking
and activation, model metadata, Skill instructions, Extension declarations,
inference request/output codecs, and inference service lifecycle.

Keep the `agent`, `function`, `model`, `skill`, `extension`, and `inference`
modules as distinct domains inside this crate. Do not recreate one workspace
crate per manifest kind.

The runtime must not own Agent scheduling, durable Agent state, CLI rendering,
or concrete builtin Tool handlers. Child-process and PTY lifecycle belongs to
`agl-execd`; runtime package resolution and inference use the typed execution
client.

Do not infer model bindings from filenames, scan arbitrary model directories,
persist credentials, or add in-process native llama.cpp linkage.

## Module map (`src/`)

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Public runtime exports. |
| `package.rs` | Bounded directory-backed package reads and package-tree validation. |
| `agent.rs`, `skill.rs` | Agent/Skill package parsing and instruction loading. |
| `extension/manifest.rs` | Extension manifest, Tool/Effect declarations and schema loading. |
| `function/manifest.rs` | `FUNCTION.toml`, dependency references, inference settings, limits and permissions. |
| `function/requirement.rs` | Typed Function requirement parsing. |
| `function/family.rs` | `FAMILY.toml` profiles and family discovery from workspace artifacts. |
| `forge.rs` | AGL workspace lookup, Function identity/dependency resolution and Forge locks/materializations. |
| `workspace_artifacts.rs` | Workspace artifact declarations, serialization/editing, output directories and resolution. |
| `activation.rs` | Dependency/package assembly, permission grants, active Function snapshots and model-service activation. |
| `model/manifest.rs`, `model/format.rs` | Model package metadata, inference dialect and format configuration. |
| `model/acquisition.rs` | Explicit model fetch/import and digest-verified managed GGUF files. |
| `inference/` | Model services, private llama-server transport and request/output codecs; see its AGENTS.md. |

Planner instructions and response schemas are in `agl-daemon::planner`.
Activation requires an acquired model; download prompting belongs to the CLI.
