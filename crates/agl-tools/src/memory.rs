use std::path::{Path, PathBuf};

use agl_memory::{
    MemoryKind, MemoryRepository, MemoryScope, MemoryScopeKind, MemorySuggestionDraft,
};
use agl_store::AglStore;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
};

pub const PROVIDER_ID: &str = "memory-tools";
pub const MEMORY_SUGGEST_TOOL_ID: &str = "memory.suggest";

#[derive(Clone, Debug)]
pub struct MemoryTools {
    store_root: PathBuf,
}

impl MemoryTools {
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
        }
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            MEMORY_SUGGEST_TOOL_ID => self.suggest(arguments),
            _ => anyhow::bail!("unknown memory tool `{name}`"),
        }
    }

    fn suggest(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<SuggestArgs>(MEMORY_SUGGEST_TOOL_ID, arguments)?;
        let scope = parse_scope(&args.scope, args.scope_key)?;
        let kind = parse_kind(&args.kind)?;
        let mut draft =
            MemorySuggestionDraft::new(scope, kind, args.title, args.body, args.source_ref);
        if let Some(confidence) = args.confidence {
            draft.confidence = confidence;
        }
        let store = self.open_store()?;
        let memory = MemoryRepository::new(&store);
        let suggestion = memory.suggest(draft)?;
        Ok(format!(
            "tool=memory.suggest\nsuggestion_id={}\nstatus={}\ntitle={}",
            suggestion.id,
            suggestion.status.as_str(),
            suggestion.title
        ))
    }

    fn open_store(&self) -> Result<AglStore> {
        AglStore::open_at(&self.store_root)
            .with_context(|| format!("failed to open memory store {}", self.store_root.display()))
    }
}

impl ToolHandler for MemoryTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin memory provider id is valid"),
        "Memory Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin memory provider declaration is valid")
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(MEMORY_SUGGEST_TOOL_ID).expect("builtin memory tool id is valid"),
            "Create a pending local memory suggestion for explicit approval.",
            ToolCapability::Write,
            ["scope", "kind", "title", "body", "source_ref"],
        )
        .with_state_effects([ToolStateEffect::StoreMemorySuggestions]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}

fn parse_scope(scope: &str, scope_key: Option<String>) -> Result<MemoryScope> {
    let kind = match scope {
        "user" => MemoryScopeKind::User,
        "repo" => MemoryScopeKind::Repo,
        "matrix_room" => MemoryScopeKind::MatrixRoom,
        "matrix_user" => MemoryScopeKind::MatrixUser,
        _ => bail!("unknown memory scope `{scope}`"),
    };
    match (kind, scope_key) {
        (MemoryScopeKind::User, None) => Ok(MemoryScope::user()),
        (kind, Some(key)) => MemoryScope::new(kind, key).map_err(anyhow::Error::from),
        (kind, None) => bail!("scope_key is required for memory scope `{}`", kind.as_str()),
    }
}

fn parse_kind(kind: &str) -> Result<MemoryKind> {
    match kind {
        "fact" => Ok(MemoryKind::Fact),
        "preference" => Ok(MemoryKind::Preference),
        "summary" => Ok(MemoryKind::Summary),
        "decision" => Ok(MemoryKind::Decision),
        "working_note" => Ok(MemoryKind::WorkingNote),
        _ => bail!("unknown memory kind `{kind}`"),
    }
}

#[derive(Deserialize)]
struct SuggestArgs {
    scope: String,
    scope_key: Option<String>,
    kind: String,
    title: String,
    body: String,
    source_ref: String,
    confidence: Option<u8>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn memory_suggest_tool_creates_pending_suggestion() {
        let root = std::env::temp_dir().join(format!("agl-memory-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tools = MemoryTools::new(&root);

        let output = tools
            .dispatch(
                MEMORY_SUGGEST_TOOL_ID,
                json!({
                    "scope": "user",
                    "kind": "decision",
                    "title": "Workflow",
                    "body": "Use pending suggestions.",
                    "source_ref": "chat:turn-1"
                }),
            )
            .unwrap();

        assert!(output.contains("tool=memory.suggest"));
        assert!(output.contains("status=pending"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn memory_declaration_registers_suggest_tool() {
        let mut catalog = ToolCatalog::new();
        register(&mut catalog).unwrap();

        assert!(
            catalog
                .tool(&ToolId::new(MEMORY_SUGGEST_TOOL_ID).unwrap())
                .is_some()
        );
    }
}
