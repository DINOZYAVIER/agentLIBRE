use std::path::Path;

use serde::{Deserialize, Serialize};

pub const DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP: usize = 1800;

/// Informational product-surface metadata; this is not an executable capability declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFeature {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub read_only_actions: &'static [&'static str],
    pub write_actions: &'static [&'static str],
    pub commands: &'static [&'static str],
    pub requires: &'static [&'static str],
    pub model_tools: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFeatureRenderOptions<'a> {
    pub version: &'a str,
    pub workspace_root: Option<&'a Path>,
    pub tool_mode: &'a str,
    pub available_model_tools: &'a [&'a str],
    pub char_cap: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeFeatureContextEvidence {
    pub feature_ids: Vec<String>,
    pub tool_mode: String,
    pub rendered_chars: usize,
    pub budget_cap_chars: usize,
    pub truncated: bool,
    pub registry_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedRuntimeFeatureContext {
    pub content: String,
    pub evidence: RuntimeFeatureContextEvidence,
}

pub fn first_party_runtime_features() -> &'static [RuntimeFeature] {
    &[
        RuntimeFeature {
            id: "cron",
            title: "Cron jobs",
            summary: "schedule builtin/trusted-skill jobs via store/daemon",
            read_only_actions: &["list", "show", "history", "preflight"],
            write_actions: &["add", "delete", "run", "tick"],
            commands: &[
                "agl cron add",
                "agl cron list",
                "agl cron show",
                "agl cron run",
                "agl cron tick",
            ],
            requires: &["agl-store", "agl-daemon for scheduled execution"],
            model_tools: &[],
        },
        RuntimeFeature {
            id: "memory",
            title: "Memory",
            summary: "scoped memory suggestions/approval/search",
            read_only_actions: &["list", "search", "list-suggestions"],
            write_actions: &["add", "suggest", "approve", "reject"],
            commands: &[
                "agl memory add",
                "agl memory suggest",
                "agl memory approve",
                "agl memory reject",
                "agl memory search",
            ],
            requires: &["agl-store"],
            model_tools: &["memory.suggest"],
        },
        RuntimeFeature {
            id: "notes",
            title: "Notes",
            summary: "SQLite notes, tombstone audit, explicit memory promotion",
            read_only_actions: &["list", "search", "show"],
            write_actions: &["add", "update", "delete", "link", "remember"],
            commands: &[
                "agl notes add",
                "agl notes list",
                "agl notes show",
                "agl notes remember",
                "agl notes delete",
            ],
            requires: &["agl-store"],
            model_tools: &["notes.add", "notes.search"],
        },
        RuntimeFeature {
            id: "store",
            title: "Store",
            summary: "SQLite migrations/idempotency/status/known-domain JSONL export",
            read_only_actions: &["status", "export"],
            write_actions: &["migrate", "record idempotency"],
            commands: &["agl store status", "agl store export"],
            requires: &["local data dir"],
            model_tools: &[],
        },
        RuntimeFeature {
            id: "skills",
            title: "Skills",
            summary: "git-verified skills with local trust/revoke",
            read_only_actions: &["list", "inspect", "status", "verify"],
            write_actions: &["lock", "trust", "revoke"],
            commands: &[
                "agl skill list",
                "agl skill inspect",
                "agl skill verify",
                "agl skill lock",
                "agl skill trust",
                "agl skill revoke",
            ],
            requires: &["clean pinned .agl/skills git component for workspace skills"],
            model_tools: &[],
        },
        RuntimeFeature {
            id: "repo",
            title: "Repo workspace",
            summary: "workspace init/status/hooks/profile",
            read_only_actions: &["status", "profile export"],
            write_actions: &["init", "install-hooks", "profile import"],
            commands: &[
                "agl init",
                "agl status",
                "agl install-hooks",
                "agl repo status",
                "agl repo export-profile",
            ],
            requires: &["workspace root"],
            model_tools: &[],
        },
        RuntimeFeature {
            id: "matrix",
            title: "Matrix",
            summary: "encrypted room/user boundary and outbox",
            read_only_actions: &["inspect configured boundary", "read outbox state"],
            write_actions: &["deliver queued notifications"],
            commands: &["agl-matrix-bridge", "agl cron tick"],
            requires: &["configured Matrix bridge"],
            model_tools: &[],
        },
        RuntimeFeature {
            id: "daemon",
            title: "Daemon",
            summary: "scheduler and bridge runtime",
            read_only_actions: &["status"],
            write_actions: &["serve", "run scheduled work"],
            commands: &["agl serve", "agl daemon status"],
            requires: &["local socket"],
            model_tools: &[],
        },
        RuntimeFeature {
            id: "permissions",
            title: "Permissions",
            summary: "inspect current grants and request exact tool access",
            read_only_actions: &["status", "request"],
            write_actions: &["grant", "revoke"],
            commands: &["agl chat --tool-mode write"],
            requires: &["agl-store for durable request/grant evidence"],
            model_tools: &["permissions.status", "permissions.request"],
        },
        RuntimeFeature {
            id: "filesystem_tools",
            title: "Filesystem tools",
            summary: "only listed repository fs tools are callable",
            read_only_actions: &["fs.list", "fs.read", "fs.search"],
            write_actions: &["fs.edit"],
            commands: &[],
            requires: &["workspace root"],
            model_tools: &["fs.list", "fs.read", "fs.search", "fs.edit"],
        },
    ]
}

pub fn render_runtime_feature_context(
    options: RuntimeFeatureRenderOptions<'_>,
) -> RenderedRuntimeFeatureContext {
    let features = first_party_runtime_features();
    let cap = if options.char_cap == 0 {
        DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP
    } else {
        options.char_cap
    };
    let mut selected = features.iter().collect::<Vec<_>>();
    let mut content = render_context(&selected, &options);
    let mut truncated = false;

    while content.chars().count() > cap && selected.len() > 5 {
        truncated = true;
        if let Some(index) = selected
            .iter()
            .rposition(|feature| !matches!(feature.id, "cron" | "filesystem_tools"))
        {
            selected.remove(index);
            content = render_context(&selected, &options);
        } else {
            break;
        }
    }

    let feature_ids = selected
        .iter()
        .map(|feature| feature.id.to_string())
        .collect::<Vec<_>>();
    let rendered_chars = content.chars().count();
    RenderedRuntimeFeatureContext {
        content,
        evidence: RuntimeFeatureContextEvidence {
            feature_ids,
            tool_mode: options.tool_mode.to_string(),
            rendered_chars,
            budget_cap_chars: cap,
            truncated,
            registry_hash: runtime_feature_registry_hash(features),
        },
    }
}

pub fn runtime_feature_registry_hash(features: &[RuntimeFeature]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for feature in features {
        hash_bytes(&mut hash, feature.id.as_bytes());
        hash_bytes(&mut hash, feature.title.as_bytes());
        hash_bytes(&mut hash, feature.summary.as_bytes());
        for field in [
            feature.read_only_actions,
            feature.write_actions,
            feature.commands,
            feature.requires,
            feature.model_tools,
        ] {
            for value in field {
                hash_bytes(&mut hash, value.as_bytes());
            }
        }
    }
    format!("fnv1a64:{hash:016x}")
}

fn render_context(
    features: &[&RuntimeFeature],
    options: &RuntimeFeatureRenderOptions<'_>,
) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_runtime_features>\n");
    content.push_str("version: agl ");
    content.push_str(options.version);
    content.push('\n');
    if let Some(workspace_root) = options.workspace_root {
        content.push_str("workspace: ");
        content.push_str(&workspace_root.display().to_string());
        content.push('\n');
    }
    content.push_str("tool_mode: ");
    content.push_str(options.tool_mode);
    content.push('\n');
    content.push_str("model_tools: ");
    if options.available_model_tools.is_empty() {
        content.push_str("none");
    } else {
        content.push_str(&options.available_model_tools.join(", "));
    }
    content.push_str("\n\nInformational runtime features:\n");
    for feature in features {
        content.push_str("- ");
        content.push_str(feature.id);
        content.push_str(": ");
        content.push_str(feature.summary);
        if !feature.read_only_actions.is_empty() {
            content.push_str("; read-only: ");
            content.push_str(&feature.read_only_actions.join(", "));
        }
        if !feature.write_actions.is_empty() {
            content.push_str("; write: ");
            content.push_str(&feature.write_actions.join(", "));
        }
        content.push_str(".\n");
    }
    if options.tool_mode == "read-only" {
        content.push_str("Read-only mode: do not offer to schedule, run, send, lock, trust, revoke, or write. If permissions.request is listed, request exact tools; otherwise explain the CLI/daemon path.\n");
    }
    content.push_str("Information only: runtime feature IDs describe available product surfaces; they are not executable capability IDs, tool names, or permissions.\n");
    content.push_str("Invocation boundary: call only exact names listed in model_tools/tool_context. Do not call cron, matrix, skills, repo, store, memory, notes, permissions, or daemon unless that exact name appears there.\n");
    content.push_str("</agentlibre_runtime_features>\n");
    content
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
    *hash ^= 0xff;
    *hash = hash.wrapping_mul(0x100000001b3);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_personal_agent_wave_surfaces() {
        let features = first_party_runtime_features();
        let by_id = |id: &str| {
            features
                .iter()
                .find(|feature| feature.id == id)
                .unwrap_or_else(|| panic!("missing feature {id}"))
        };

        assert!(by_id("cron").commands.contains(&"agl cron run"));
        assert!(by_id("cron").commands.contains(&"agl cron tick"));
        assert!(by_id("memory").summary.contains("suggestions"));
        assert!(by_id("notes").summary.contains("tombstone audit"));
        assert!(by_id("store").summary.contains("idempotency"));
        assert!(by_id("skills").commands.contains(&"agl skill revoke"));
    }

    #[test]
    fn rendered_context_is_explicitly_informational() {
        let tool_names = ["fs.list", "fs.read", "fs.search"];
        let rendered = render_runtime_feature_context(RuntimeFeatureRenderOptions {
            version: "1.0.0-alpha.test",
            workspace_root: Some(Path::new("/repo")),
            tool_mode: "read-only",
            available_model_tools: &tool_names,
            char_cap: DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP,
        });

        assert!(rendered.content.contains("<agentlibre_runtime_features>"));
        assert!(rendered.content.contains("version: agl 1.0.0-alpha.test"));
        assert!(rendered.content.contains("workspace: /repo"));
        assert!(rendered.content.contains("tool_mode: read-only"));
        assert!(
            rendered
                .content
                .contains("model_tools: fs.list, fs.read, fs.search")
        );
        assert!(rendered.content.contains("- cron:"));
        assert!(
            rendered
                .content
                .contains("read-only: list, show, history, preflight")
        );
        assert!(rendered.content.contains("write: add, delete, run, tick"));
        assert!(rendered.content.contains("Informational runtime features:"));
        assert!(rendered.content.contains("Information only:"));
        assert!(rendered.content.contains("not executable capability IDs"));
        assert!(
            rendered
                .content
                .contains("Do not call cron, matrix, skills")
        );
        assert!(
            rendered
                .content
                .contains("Read-only mode: do not offer to schedule")
        );
        assert!(rendered.evidence.feature_ids.contains(&"cron".to_string()));
        assert_eq!(rendered.evidence.tool_mode, "read-only");
        assert!(!rendered.evidence.registry_hash.is_empty());
    }
}
