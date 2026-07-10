use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_memory::{
    MemoryDraft, MemoryKind, MemoryRepository, MemoryScope, MemoryScopeKind, MemorySearchQuery,
    MemorySuggestionDraft,
};
use agl_store::AglStore;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ToolCatalog, ToolCatalogError, parse_action_args as parse_args};

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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
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

    fn search(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<SearchArgs>(MEMORY_SEARCH_TOOL_ID, arguments)?;
        let store = self.open_store_read_only()?;
        let memory = MemoryRepository::new(&store);
        let entries = memory.search(&MemorySearchQuery {
            scope: parse_optional_scope(
                args.scope.as_ref().map(MemoryScopeArg::as_str),
                args.scope_key,
            )?,
            text: Some(args.query),
            include_deleted: args.include_deleted.unwrap_or(false),
            limit: bounded_limit(args.limit),
        })?;
        Ok(render_entries(MEMORY_SEARCH_TOOL_ID, entries))
    }

    fn list(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<ListArgs>(MEMORY_LIST_TOOL_ID, arguments)?;
        let store = self.open_store_read_only()?;
        let memory = MemoryRepository::new(&store);
        let entries = memory.list(&MemorySearchQuery {
            scope: parse_optional_scope(
                args.scope.as_ref().map(MemoryScopeArg::as_str),
                args.scope_key,
            )?,
            text: None,
            include_deleted: args.include_deleted.unwrap_or(false),
            limit: bounded_limit(args.limit),
        })?;
        Ok(render_entries(MEMORY_LIST_TOOL_ID, entries))
    }

    fn suggest(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<SuggestArgs>(MEMORY_SUGGEST_TOOL_ID, arguments)?;
        let scope = parse_scope(args.scope.as_str(), args.scope_key)?;
        let kind = parse_kind(args.kind.as_str())?;
        let mut draft =
            MemorySuggestionDraft::new(scope, kind, args.title, args.body, args.source_ref);
        if let Some(confidence) = args.confidence {
            draft.confidence = confidence;
        }
        let store = self.open_store_writable()?;
        let memory = MemoryRepository::new(&store);
        let suggestion = memory.suggest(draft)?;
        Ok(json!({
            "tool": MEMORY_SUGGEST_TOOL_ID,
            "suggestion_id": suggestion.id,
            "status": suggestion.status.as_str(),
            "title": suggestion.title,
        }))
    }

    fn add(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<AddArgs>(MEMORY_ADD_TOOL_ID, arguments)?;
        let scope = parse_scope(args.scope.as_str(), args.scope_key)?;
        let kind = parse_kind(args.kind.as_str())?;
        let mut draft = MemoryDraft::new(scope, kind, args.title, args.body);
        draft.source_ref = args.source_ref;
        if let Some(confidence) = args.confidence {
            draft.confidence = confidence;
        }
        let store = self.open_store_writable()?;
        let memory = MemoryRepository::new(&store);
        let entry = memory.add(draft)?;
        Ok(json!({
            "tool": MEMORY_ADD_TOOL_ID,
            "memory_id": entry.id,
            "scope": {
                "kind": entry.scope.kind.as_str(),
                "key": entry.scope.key,
            },
            "kind": entry.kind.as_str(),
            "title": entry.title,
            "status": "created",
        }))
    }

    fn approve(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<SuggestionIdArgs>(MEMORY_APPROVE_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let memory = MemoryRepository::new(&store);
        let (suggestion, entry) = memory.approve_suggestion(&args.suggestion_id)?;
        Ok(json!({
            "tool": MEMORY_APPROVE_TOOL_ID,
            "suggestion_id": suggestion.id,
            "memory_id": entry.id,
            "status": suggestion.status.as_str(),
        }))
    }

    fn reject(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<RejectArgs>(MEMORY_REJECT_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let memory = MemoryRepository::new(&store);
        let suggestion =
            memory.reject_suggestion(&args.suggestion_id, args.resolution_note.as_deref())?;
        Ok(json!({
            "tool": MEMORY_REJECT_TOOL_ID,
            "suggestion_id": suggestion.id,
            "status": suggestion.status.as_str(),
        }))
    }

    fn open_store_read_only(&self) -> Result<AglStore> {
        AglStore::open_current_read_only_at(&self.store_root)
            .with_context(|| format!("failed to open memory store {}", self.store_root.display()))
    }

    fn open_store_writable(&self) -> Result<AglStore> {
        AglStore::open_current_at(&self.store_root)
            .with_context(|| format!("failed to open memory store {}", self.store_root.display()))
    }
}

impl ActionHandler for MemoryTools {
    fn dispatch(
        &self,
        invocation: ActionInvocation,
    ) -> std::result::Result<ActionResult, ActionHandlerError> {
        let data = self.dispatch(invocation.capability_id.as_str(), invocation.arguments)?;
        Ok(ActionResult::new(data))
    }
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin memory provider id is valid"),
        "Memory Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin memory provider declaration is valid")
    .with_action(action::<SearchArgs>(
        MEMORY_SEARCH_TOOL_ID,
        "Search approved local memories by scope and text.",
        OperationKind::Read,
    ))
    .with_action(action::<ListArgs>(
        MEMORY_LIST_TOOL_ID,
        "List approved local memories by optional scope.",
        OperationKind::Read,
    ))
    .with_action(
        action::<SuggestArgs>(
            MEMORY_SUGGEST_TOOL_ID,
            "Create a pending local memory suggestion for explicit approval.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::StoreMemorySuggestions]),
    )
    .with_action(
        action::<AddArgs>(
            MEMORY_ADD_TOOL_ID,
            "Create an approved local memory entry.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::StoreMemoryEntries]),
    )
    .with_action(
        action::<SuggestionIdArgs>(
            MEMORY_APPROVE_TOOL_ID,
            "Approve a pending memory suggestion into a durable memory entry.",
            OperationKind::Approve,
        )
        .with_state_effects([
            StateEffect::StoreMemorySuggestions,
            StateEffect::StoreMemoryEntries,
        ]),
    )
    .with_action(
        action::<RejectArgs>(
            MEMORY_REJECT_TOOL_ID,
            "Reject a pending memory suggestion.",
            OperationKind::Approve,
        )
        .with_state_effects([StateEffect::StoreMemorySuggestions]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
) -> ActionDeclaration {
    ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("builtin memory tool id is valid"),
        description,
        operation_kind,
    )
    .expect("builtin memory tool declaration schema is valid")
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

fn render_entries(tool_id: &str, entries: Vec<agl_memory::MemoryEntry>) -> Value {
    let entries = entries
        .into_iter()
        .map(|entry| {
            json!({
                "memory_id": entry.id,
                "scope": {
                    "kind": entry.scope.kind.as_str(),
                    "key": entry.scope.key,
                },
                "kind": entry.kind.as_str(),
                "title": entry.title,
                "deleted": entry.deleted_at.is_some(),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "tool": tool_id,
        "status": "ok",
        "entry_count": entries.len(),
        "entries": entries,
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoryScopeArg {
    User,
    Repo,
    MatrixRoom,
    MatrixUser,
}

impl MemoryScopeArg {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Repo => "repo",
            Self::MatrixRoom => "matrix_room",
            Self::MatrixUser => "matrix_user",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoryKindArg {
    Fact,
    Preference,
    Summary,
    Decision,
    WorkingNote,
}

impl MemoryKindArg {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Summary => "summary",
            Self::Decision => "decision",
            Self::WorkingNote => "working_note",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    scope: Option<MemoryScopeArg>,
    scope_key: Option<String>,
    limit: Option<usize>,
    include_deleted: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    scope: Option<MemoryScopeArg>,
    scope_key: Option<String>,
    limit: Option<usize>,
    include_deleted: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SuggestArgs {
    scope: MemoryScopeArg,
    scope_key: Option<String>,
    kind: MemoryKindArg,
    title: String,
    body: String,
    source_ref: String,
    confidence: Option<u8>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    scope: MemoryScopeArg,
    scope_key: Option<String>,
    kind: MemoryKindArg,
    title: String,
    body: String,
    source_ref: Option<String>,
    confidence: Option<u8>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SuggestionIdArgs {
    suggestion_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RejectArgs {
    suggestion_id: String,
    resolution_note: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::migrated_temp_root;

    use super::*;

    #[test]
    fn memory_suggest_tool_creates_pending_suggestion() {
        let root = migrated_temp_root("suggest");
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

        assert_eq!(output["tool"], MEMORY_SUGGEST_TOOL_ID);
        assert_eq!(output["status"], "pending");
        assert_eq!(output["title"], "Workflow");
    }

    #[test]
    fn memory_tools_add_search_approve_and_reject() {
        let root = migrated_temp_root("lifecycle");
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

        assert_eq!(add["status"], "created");
        assert_eq!(add["scope"], json!({"kind": "user", "key": "default"}));
        assert_eq!(list["entry_count"], 1);
        assert_eq!(search["entries"][0]["title"], "Workflow");

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
        let suggestion_id = pending["suggestion_id"].as_str().unwrap();
        let approved = tools
            .dispatch(
                MEMORY_APPROVE_TOOL_ID,
                json!({"suggestion_id": suggestion_id}),
            )
            .unwrap();
        assert_eq!(approved["status"], "approved");
        assert!(approved["memory_id"].is_string());

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
        let suggestion_id = pending["suggestion_id"].as_str().unwrap();
        let rejected = tools
            .dispatch(
                MEMORY_REJECT_TOOL_ID,
                json!({
                    "suggestion_id": suggestion_id,
                    "resolution_note": "not needed"
                }),
            )
            .unwrap();
        assert_eq!(rejected["status"], "rejected");
    }

    #[test]
    fn memory_declaration_registers_suggest_tool() {
        let declaration = declaration();
        declaration.validate().unwrap();
        let suggest = declaration
            .action(&CapabilityId::new(MEMORY_SUGGEST_TOOL_ID).unwrap())
            .unwrap();
        assert_eq!(suggest.input_schema["additionalProperties"], false);
        let schema = suggest.compile_schema().unwrap();
        let valid = json!({
            "scope": "user",
            "kind": "decision",
            "title": "Workflow",
            "body": "Use pending suggestions.",
            "source_ref": "chat:turn-1"
        });
        assert!(schema.validate(&valid).is_ok());
        assert!(schema.validate(&json!({})).is_err());
        assert!(
            schema
                .validate(&json!({
                    "scope": "user",
                    "kind": "decision",
                    "title": "Workflow",
                    "body": "Use pending suggestions.",
                    "source_ref": "chat:turn-1",
                    "extra": true
                }))
                .is_err()
        );
        assert!(
            schema
                .validate(&json!({
                    "scope": "user",
                    "kind": "decision",
                    "title": "Workflow",
                    "body": "Use pending suggestions.",
                    "source_ref": 7
                }))
                .is_err()
        );
    }
}
