use std::path::{Path, PathBuf};

use agl_memory::{
    MemoryDraft, MemoryKind, MemoryRepository, MemoryScope, MemoryScopeKind, MemorySearchQuery,
    MemorySuggestionDraft,
};
use agl_store::AglStore;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOperationKind, ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
    parse_tool_args as parse_args,
};

pub const PROVIDER_ID: &str = "memory-tools";
pub const MEMORY_SEARCH_TOOL_ID: &str = "memory.search";
pub const MEMORY_LIST_TOOL_ID: &str = "memory.list";
pub const MEMORY_SUGGEST_TOOL_ID: &str = "memory.suggest";
pub const MEMORY_ADD_TOOL_ID: &str = "memory.add";
pub const MEMORY_APPROVE_TOOL_ID: &str = "memory.approve";
pub const MEMORY_REJECT_TOOL_ID: &str = "memory.reject";

const DEFAULT_LIST_LIMIT: usize = 10;
const MAX_LIST_LIMIT: usize = 50;

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
            MEMORY_SEARCH_TOOL_ID => self.search(arguments),
            MEMORY_LIST_TOOL_ID => self.list(arguments),
            MEMORY_SUGGEST_TOOL_ID => self.suggest(arguments),
            MEMORY_ADD_TOOL_ID => self.add(arguments),
            MEMORY_APPROVE_TOOL_ID => self.approve(arguments),
            MEMORY_REJECT_TOOL_ID => self.reject(arguments),
            _ => anyhow::bail!("unknown memory tool `{name}`"),
        }
    }

    fn search(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<SearchArgs>(MEMORY_SEARCH_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let memory = MemoryRepository::new(&store);
        let entries = memory.search(&MemorySearchQuery {
            scope: parse_optional_scope(args.scope.as_deref(), args.scope_key)?,
            text: Some(args.query),
            include_deleted: args.include_deleted.unwrap_or(false),
            limit: bounded_limit(args.limit),
        })?;
        Ok(render_entries(MEMORY_SEARCH_TOOL_ID, entries))
    }

    fn list(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<ListArgs>(MEMORY_LIST_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let memory = MemoryRepository::new(&store);
        let entries = memory.list(&MemorySearchQuery {
            scope: parse_optional_scope(args.scope.as_deref(), args.scope_key)?,
            text: None,
            include_deleted: args.include_deleted.unwrap_or(false),
            limit: bounded_limit(args.limit),
        })?;
        Ok(render_entries(MEMORY_LIST_TOOL_ID, entries))
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

    fn add(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<AddArgs>(MEMORY_ADD_TOOL_ID, arguments)?;
        let scope = parse_scope(&args.scope, args.scope_key)?;
        let kind = parse_kind(&args.kind)?;
        let mut draft = MemoryDraft::new(scope, kind, args.title, args.body);
        draft.source_ref = args.source_ref;
        if let Some(confidence) = args.confidence {
            draft.confidence = confidence;
        }
        let store = self.open_store()?;
        let memory = MemoryRepository::new(&store);
        let entry = memory.add(draft)?;
        Ok(format!(
            "tool=memory.add\nmemory_id={}\nscope={}:{}\nkind={}\ntitle={}\nstatus=created",
            entry.id,
            entry.scope.kind.as_str(),
            entry.scope.key,
            entry.kind.as_str(),
            entry.title
        ))
    }

    fn approve(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<SuggestionIdArgs>(MEMORY_APPROVE_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let memory = MemoryRepository::new(&store);
        let (suggestion, entry) = memory.approve_suggestion(&args.suggestion_id)?;
        Ok(format!(
            "tool=memory.approve\nsuggestion_id={}\nmemory_id={}\nstatus={}",
            suggestion.id,
            entry.id,
            suggestion.status.as_str()
        ))
    }

    fn reject(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<RejectArgs>(MEMORY_REJECT_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let memory = MemoryRepository::new(&store);
        let suggestion =
            memory.reject_suggestion(&args.suggestion_id, args.resolution_note.as_deref())?;
        Ok(format!(
            "tool=memory.reject\nsuggestion_id={}\nstatus={}",
            suggestion.id,
            suggestion.status.as_str()
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
    .with_tool(ToolDeclaration::new(
        ToolId::new(MEMORY_SEARCH_TOOL_ID).expect("builtin memory tool id is valid"),
        "Search approved local memories by scope and text.",
        ToolCapability::Read,
        ["query"],
    ))
    .with_tool(ToolDeclaration::new(
        ToolId::new(MEMORY_LIST_TOOL_ID).expect("builtin memory tool id is valid"),
        "List approved local memories by optional scope.",
        ToolCapability::Read,
        std::iter::empty::<&str>(),
    ))
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(MEMORY_SUGGEST_TOOL_ID).expect("builtin memory tool id is valid"),
            "Create a pending local memory suggestion for explicit approval.",
            ToolCapability::Write,
            ["scope", "kind", "title", "body", "source_ref"],
        )
        .with_state_effects([ToolStateEffect::StoreMemorySuggestions]),
    )
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(MEMORY_ADD_TOOL_ID).expect("builtin memory tool id is valid"),
            "Create an approved local memory entry.",
            ToolCapability::Write,
            ["scope", "kind", "title", "body"],
        )
        .with_state_effects([ToolStateEffect::StoreMemoryEntries]),
    )
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(MEMORY_APPROVE_TOOL_ID).expect("builtin memory tool id is valid"),
            "Approve a pending memory suggestion into a durable memory entry.",
            ToolCapability::Write,
            ["suggestion_id"],
        )
        .with_operation_kind(ToolOperationKind::Approve)
        .with_state_effects([
            ToolStateEffect::StoreMemorySuggestions,
            ToolStateEffect::StoreMemoryEntries,
        ]),
    )
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(MEMORY_REJECT_TOOL_ID).expect("builtin memory tool id is valid"),
            "Reject a pending memory suggestion.",
            ToolCapability::Write,
            ["suggestion_id"],
        )
        .with_operation_kind(ToolOperationKind::Approve)
        .with_state_effects([ToolStateEffect::StoreMemorySuggestions]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn parse_scope(scope: &str, scope_key: Option<String>) -> Result<MemoryScope> {
    let kind = parse_scope_kind(scope)?;
    match (kind, scope_key) {
        (MemoryScopeKind::User, None) => Ok(MemoryScope::user()),
        (kind, Some(key)) => MemoryScope::new(kind, key).map_err(anyhow::Error::from),
        (kind, None) => bail!("scope_key is required for memory scope `{}`", kind.as_str()),
    }
}

fn parse_optional_scope(
    scope: Option<&str>,
    scope_key: Option<String>,
) -> Result<Option<MemoryScope>> {
    scope.map(|scope| parse_scope(scope, scope_key)).transpose()
}

pub(crate) fn parse_scope_kind(scope: &str) -> Result<MemoryScopeKind> {
    match scope {
        "user" => Ok(MemoryScopeKind::User),
        "repo" => Ok(MemoryScopeKind::Repo),
        "matrix_room" => Ok(MemoryScopeKind::MatrixRoom),
        "matrix_user" => Ok(MemoryScopeKind::MatrixUser),
        _ => bail!("unknown memory scope `{scope}`"),
    }
}

pub(crate) fn parse_kind(kind: &str) -> Result<MemoryKind> {
    match kind {
        "fact" => Ok(MemoryKind::Fact),
        "preference" => Ok(MemoryKind::Preference),
        "summary" => Ok(MemoryKind::Summary),
        "decision" => Ok(MemoryKind::Decision),
        "working_note" => Ok(MemoryKind::WorkingNote),
        _ => bail!("unknown memory kind `{kind}`"),
    }
}

fn bounded_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT)
}

fn render_entries(tool_id: &str, entries: Vec<agl_memory::MemoryEntry>) -> String {
    let mut output = format!("tool={tool_id}\nentries={}\n---", entries.len());
    for entry in entries {
        output.push('\n');
        output.push_str(&format!(
            "memory id={} scope={}:{} kind={} title={} deleted={}",
            entry.id,
            entry.scope.kind.as_str(),
            entry.scope.key,
            entry.kind.as_str(),
            entry.title,
            entry.deleted_at.is_some()
        ));
    }
    output
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    scope: Option<String>,
    scope_key: Option<String>,
    limit: Option<usize>,
    include_deleted: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    scope: Option<String>,
    scope_key: Option<String>,
    limit: Option<usize>,
    include_deleted: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestArgs {
    scope: String,
    scope_key: Option<String>,
    kind: String,
    title: String,
    body: String,
    source_ref: String,
    confidence: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    scope: String,
    scope_key: Option<String>,
    kind: String,
    title: String,
    body: String,
    source_ref: Option<String>,
    confidence: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestionIdArgs {
    suggestion_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectArgs {
    suggestion_id: String,
    resolution_note: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::{temp_root, value_for};

    use super::*;

    #[test]
    fn memory_suggest_tool_creates_pending_suggestion() {
        let root = temp_root("suggest");
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
    }

    #[test]
    fn memory_tools_add_search_approve_and_reject() {
        let root = temp_root("lifecycle");
        let tools = MemoryTools::new(&root);

        let add = tools
            .dispatch(
                MEMORY_ADD_TOOL_ID,
                json!({
                    "scope": "user",
                    "kind": "preference",
                    "title": "Workflow",
                    "body": "Prefer explicit approvals."
                }),
            )
            .unwrap();
        let list = tools
            .dispatch(MEMORY_LIST_TOOL_ID, json!({"scope": "user"}))
            .unwrap();
        let search = tools
            .dispatch(MEMORY_SEARCH_TOOL_ID, json!({"query": "approvals"}))
            .unwrap();

        assert!(add.contains("status=created"));
        assert!(list.contains("entries=1"));
        assert!(search.contains("Workflow"));

        let pending = tools
            .dispatch(
                MEMORY_SUGGEST_TOOL_ID,
                json!({
                    "scope": "user",
                    "kind": "decision",
                    "title": "Approve me",
                    "body": "Promote this.",
                    "source_ref": "test:approve"
                }),
            )
            .unwrap();
        let suggestion_id = value_for(&pending, "suggestion_id=").unwrap();
        let approved = tools
            .dispatch(
                MEMORY_APPROVE_TOOL_ID,
                json!({"suggestion_id": suggestion_id}),
            )
            .unwrap();
        assert!(approved.contains("status=approved"));
        assert!(approved.contains("memory_id="));

        let pending = tools
            .dispatch(
                MEMORY_SUGGEST_TOOL_ID,
                json!({
                    "scope": "repo",
                    "scope_key": "agentLIBRE",
                    "kind": "working_note",
                    "title": "Reject me",
                    "body": "Do not promote this.",
                    "source_ref": "test:reject"
                }),
            )
            .unwrap();
        let suggestion_id = value_for(&pending, "suggestion_id=").unwrap();
        let rejected = tools
            .dispatch(
                MEMORY_REJECT_TOOL_ID,
                json!({
                    "suggestion_id": suggestion_id,
                    "resolution_note": "not needed"
                }),
            )
            .unwrap();
        assert!(rejected.contains("status=rejected"));
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
