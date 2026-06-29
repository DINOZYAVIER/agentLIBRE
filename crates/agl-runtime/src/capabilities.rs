use std::path::Path;

use serde::{Deserialize, Serialize};

pub const DEFAULT_RUNTIME_CAPABILITY_CONTEXT_CHAR_CAP: usize = 1800;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCapability {
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
pub struct RuntimeCapabilityRenderOptions<'a> {
    pub version: &'a str,
    pub workspace_root: Option<&'a Path>,
    pub tool_mode: &'a str,
    pub available_model_tools: &'a [&'a str],
    pub char_cap: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCapabilityContextEvidence {
    pub capability_ids: Vec<String>,
    pub tool_mode: String,
    pub rendered_chars: usize,
    pub budget_cap_chars: usize,
    pub truncated: bool,
    pub registry_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedRuntimeCapabilityContext {
    pub content: String,
    pub evidence: RuntimeCapabilityContextEvidence,
}

pub fn first_party_runtime_capabilities() -> &'static [RuntimeCapability] {
    &[
        RuntimeCapability {
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
        RuntimeCapability {
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
        RuntimeCapability {
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
        RuntimeCapability {
            id: "store",
            title: "Store",
            summary: "SQLite migrations/idempotency/status/known-domain JSONL export",
            read_only_actions: &["status", "export"],
            write_actions: &["migrate", "record idempotency"],
            commands: &["agl store status", "agl store export"],
            requires: &["local data dir"],
            model_tools: &[],
        },
        RuntimeCapability {
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
        RuntimeCapability {
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
        RuntimeCapability {
            id: "matrix",
            title: "Matrix",
            summary: "encrypted room/user boundary and outbox",
            read_only_actions: &["inspect configured boundary", "read outbox state"],
            write_actions: &["deliver queued notifications"],
            commands: &["agl-matrix-bridge", "agl cron tick"],
            requires: &["configured Matrix bridge"],
            model_tools: &[],
        },
        RuntimeCapability {
            id: "daemon",
            title: "Daemon",
            summary: "scheduler and bridge runtime",
            read_only_actions: &["status"],
            write_actions: &["serve", "run scheduled work"],
            commands: &["agl serve", "agl daemon status"],
            requires: &["local socket"],
            model_tools: &[],
        },
        RuntimeCapability {
            id: "permissions",
            title: "Permissions",
            summary: "inspect current grants and request exact tool access",
            read_only_actions: &["status", "request"],
            write_actions: &["grant", "revoke"],
            commands: &["agl chat --tool-mode write"],
            requires: &["agl-store for durable request/grant evidence"],
            model_tools: &["permissions.status", "permissions.request"],
        },
        RuntimeCapability {
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

pub fn render_runtime_capability_context(
    options: RuntimeCapabilityRenderOptions<'_>,
) -> RenderedRuntimeCapabilityContext {
    let capabilities = first_party_runtime_capabilities();
    let cap = if options.char_cap == 0 {
        DEFAULT_RUNTIME_CAPABILITY_CONTEXT_CHAR_CAP
    } else {
        options.char_cap
    };
    let mut selected = capabilities.iter().collect::<Vec<_>>();
    let mut content = render_context(&selected, &options);
    let mut truncated = false;

    while content.chars().count() > cap && selected.len() > 5 {
        truncated = true;
        if let Some(index) = selected
            .iter()
            .rposition(|capability| !matches!(capability.id, "cron" | "filesystem_tools"))
        {
            selected.remove(index);
            content = render_context(&selected, &options);
        } else {
            break;
        }
    }

    let capability_ids = selected
        .iter()
        .map(|capability| capability.id.to_string())
        .collect::<Vec<_>>();
    RenderedRuntimeCapabilityContext {
        content,
        evidence: RuntimeCapabilityContextEvidence {
            capability_ids,
            tool_mode: options.tool_mode.to_string(),
            rendered_chars: selected_context_len(&selected, &options),
            budget_cap_chars: cap,
            truncated,
            registry_hash: runtime_capability_registry_hash(capabilities),
        },
    }
}

pub fn runtime_capability_registry_hash(capabilities: &[RuntimeCapability]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for capability in capabilities {
        hash_bytes(&mut hash, capability.id.as_bytes());
        hash_bytes(&mut hash, capability.title.as_bytes());
        hash_bytes(&mut hash, capability.summary.as_bytes());
        for field in [
            capability.read_only_actions,
            capability.write_actions,
            capability.commands,
            capability.requires,
            capability.model_tools,
        ] {
            for value in field {
                hash_bytes(&mut hash, value.as_bytes());
            }
        }
    }
    format!("fnv1a64:{hash:016x}")
}

fn selected_context_len(
    capabilities: &[&RuntimeCapability],
    options: &RuntimeCapabilityRenderOptions<'_>,
) -> usize {
    render_context(capabilities, options).chars().count()
}

fn render_context(
    capabilities: &[&RuntimeCapability],
    options: &RuntimeCapabilityRenderOptions<'_>,
) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_runtime_capabilities>\n");
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
    content.push_str("\n\nCapabilities:\n");
    for capability in capabilities {
        content.push_str("- ");
        content.push_str(capability.id);
        content.push_str(": ");
        content.push_str(capability.summary);
        if !capability.read_only_actions.is_empty() {
            content.push_str("; read-only: ");
            content.push_str(&capability.read_only_actions.join(", "));
        }
        if !capability.write_actions.is_empty() {
            content.push_str("; write: ");
            content.push_str(&capability.write_actions.join(", "));
        }
        content.push_str(".\n");
    }
    if options.tool_mode == "read-only" {
        content.push_str("Read-only mode: do not offer to schedule, run, send, lock, trust, revoke, or write. If permissions.request is listed, request exact tools; otherwise explain the CLI/daemon path.\n");
    }
    content.push_str("Boundary: capability IDs are not tool names. Do not call cron, matrix, skills, repo, store, memory, notes, permissions, or daemon unless that exact name appears in agentlibre_tool_context.\n");
    content.push_str(
        "Policy: capabilities are not permissions; call only model_tools/tool_context.\n",
    );
    content.push_str("</agentlibre_runtime_capabilities>\n");
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
        let capabilities = first_party_runtime_capabilities();
        let by_id = |id: &str| {
            capabilities
                .iter()
                .find(|capability| capability.id == id)
                .unwrap_or_else(|| panic!("missing capability {id}"))
        };

        assert!(by_id("cron").commands.contains(&"agl cron run"));
        assert!(by_id("cron").commands.contains(&"agl cron tick"));
        assert!(by_id("memory").summary.contains("suggestions"));
        assert!(by_id("notes").summary.contains("tombstone audit"));
        assert!(by_id("store").summary.contains("idempotency"));
        assert!(by_id("skills").commands.contains(&"agl skill revoke"));
    }

    #[test]
    fn rendered_context_separates_capabilities_from_permissions() {
        let tool_names = ["fs.list", "fs.read", "fs.search"];
        let rendered = render_runtime_capability_context(RuntimeCapabilityRenderOptions {
            version: "1.0.0-alpha.test",
            workspace_root: Some(Path::new("/repo")),
            tool_mode: "read-only",
            available_model_tools: &tool_names,
            char_cap: DEFAULT_RUNTIME_CAPABILITY_CONTEXT_CHAR_CAP,
        });

        assert!(
            rendered
                .content
                .contains("<agentlibre_runtime_capabilities>")
        );
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
        assert!(
            rendered
                .content
                .contains("capabilities are not permissions")
        );
        assert!(
            rendered
                .content
                .contains("capability IDs are not tool names")
        );
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
        assert!(
            rendered
                .evidence
                .capability_ids
                .contains(&"cron".to_string())
        );
        assert_eq!(rendered.evidence.tool_mode, "read-only");
        assert!(!rendered.evidence.registry_hash.is_empty());
    }
}
