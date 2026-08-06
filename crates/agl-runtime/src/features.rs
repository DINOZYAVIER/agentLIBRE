use std::collections::BTreeSet;
use std::path::Path;

use agl_kernel::ExtensionDescriptor;
use serde::{Deserialize, Serialize};

pub const DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP: usize = 512;

/// Informational product-surface metadata; this is not an executable tool declaration.
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
    pub extension_descriptors: &'a [ExtensionDescriptor],
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
            model_tools: &["core.memory:suggest"],
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
            model_tools: &["core.note:add", "core.note:search"],
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
            write_actions: &["trust", "revoke"],
            commands: &[
                "agl skill list",
                "agl skill inspect",
                "agl skill verify",
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
            commands: &["agl --mode write"],
            requires: &["agl-store for durable request/grant evidence"],
            model_tools: &["core.permission:status", "core.permission:request"],
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
    let available_tools = options
        .available_model_tools
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut feature_ids = features
        .iter()
        .filter(|feature| {
            feature
                .model_tools
                .iter()
                .any(|tool| available_tools.contains(tool))
        })
        .map(|feature| feature.id.to_string())
        .collect::<Vec<_>>();
    feature_ids.extend(
        options
            .extension_descriptors
            .iter()
            .filter(|extension| {
                extension
                    .tools
                    .iter()
                    .any(|tool| available_tools.contains(tool.id.as_str()))
            })
            .map(|extension| format!("extension:{}", extension.id)),
    );
    feature_ids.sort();
    feature_ids.dedup();
    let content = render_context(&options);
    let rendered_chars = content.chars().count();
    RenderedRuntimeFeatureContext {
        content,
        evidence: RuntimeFeatureContextEvidence {
            feature_ids,
            tool_mode: options.tool_mode.to_string(),
            rendered_chars,
            budget_cap_chars: cap,
            truncated: rendered_chars > cap,
            registry_hash: runtime_feature_registry_hash_with_extensions(
                features,
                options.extension_descriptors,
            ),
        },
    }
}

fn runtime_feature_registry_hash_with_extensions(
    features: &[RuntimeFeature],
    extensions: &[ExtensionDescriptor],
) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    hash_bytes(
        &mut hash,
        runtime_feature_registry_hash(features).as_bytes(),
    );
    let mut descriptors = extensions
        .iter()
        .map(|extension| (extension.id.as_str(), extension.digest()))
        .collect::<Vec<_>>();
    descriptors.sort_by_key(|(id, _)| *id);
    for (id, digest) in descriptors {
        hash_bytes(&mut hash, id.as_bytes());
        hash_bytes(&mut hash, digest.as_str().as_bytes());
    }
    format!("fnv1a64:{hash:016x}")
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

fn render_context(options: &RuntimeFeatureRenderOptions<'_>) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_runtime>\n");
    content.push_str("version: agl ");
    content.push_str(options.version);
    content.push('\n');
    if options.workspace_root.is_some() {
        content.push_str("workspace: active\n");
    }
    content.push_str("tool_mode: ");
    content.push_str(options.tool_mode);
    content.push('\n');
    content.push_str(
        "Only the tool schemas supplied for this turn are callable. Other product surfaces are not loaded.\n",
    );
    content.push_str("</agentlibre_runtime>\n");
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
        let tool_names = [
            "core.workspace:fs.list",
            "core.workspace:fs.read",
            "core.workspace:fs.search",
        ];
        let workspace_extension = agl_kernel::ExtensionDescriptor::builtin(
            agl_kernel::ExtensionId::new("core.workspace").unwrap(),
            "Core Workspace",
            "1.0.0",
        )
        .unwrap()
        .with_tool(
            agl_kernel::ToolDeclaration::new(
                agl_kernel::ToolId::new("core.workspace:fs.read").unwrap(),
                "Read",
                serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "additionalProperties": false
                }),
                agl_kernel::OperationKind::Read,
            )
            .unwrap(),
        );
        let rendered = render_runtime_feature_context(RuntimeFeatureRenderOptions {
            version: "1.0.0-alpha.test",
            workspace_root: Some(Path::new("/repo")),
            tool_mode: "read-only",
            available_model_tools: &tool_names,
            extension_descriptors: &[workspace_extension],
            char_cap: DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP,
        });

        assert!(rendered.content.contains("<agentlibre_runtime>"));
        assert!(rendered.content.contains("version: agl 1.0.0-alpha.test"));
        assert!(rendered.content.contains("workspace: active"));
        assert!(rendered.content.contains("tool_mode: read-only"));
        assert!(
            rendered
                .content
                .contains("Only the tool schemas supplied for this turn are callable")
        );
        assert!(!rendered.content.contains("model_tools:"));
        assert!(!rendered.content.contains("cron"));
        assert!(!rendered.content.contains("memory"));
        assert_eq!(
            rendered.evidence.feature_ids,
            vec!["extension:core.workspace".to_owned()]
        );
        assert_eq!(rendered.evidence.tool_mode, "read-only");
        assert!(rendered.evidence.rendered_chars <= DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP);
        assert!(!rendered.evidence.truncated);
        assert!(!rendered.evidence.registry_hash.is_empty());
    }
}
