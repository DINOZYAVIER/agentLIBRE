use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_notes::{NoteRepository, NoteSearchQuery, NoteUpdate};
use agl_store::AglStore;
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    ToolCatalog, ToolCatalogError,
    memory::{
        MemoryKindArg, MemoryScopeArg, parse_kind as parse_memory_kind,
        parse_scope_kind as parse_memory_scope_kind,
    },
    parse_action_args as parse_args,
};

pub const PROVIDER_ID: &str = "notes-tools";
pub const NOTES_ADD_TOOL_ID: &str = "notes.add";
pub const NOTES_SEARCH_TOOL_ID: &str = "notes.search";
pub const NOTES_SHOW_TOOL_ID: &str = "notes.show";
pub const NOTES_UPDATE_TOOL_ID: &str = "notes.update";
pub const NOTES_LINK_TOOL_ID: &str = "notes.link";
pub const NOTES_DELETE_TOOL_ID: &str = "notes.delete";
pub const NOTES_REMEMBER_TOOL_ID: &str = "notes.remember";

const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 50;

#[derive(Clone, Debug)]
pub struct NotesTools {
    store_root: PathBuf,
}

impl NotesTools {
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
        }
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            NOTES_ADD_TOOL_ID => self.add(arguments),
            NOTES_SEARCH_TOOL_ID => self.search(arguments),
            NOTES_SHOW_TOOL_ID => self.show(arguments),
            NOTES_UPDATE_TOOL_ID => self.update(arguments),
            NOTES_LINK_TOOL_ID => self.link(arguments),
            NOTES_DELETE_TOOL_ID => self.delete(arguments),
            NOTES_REMEMBER_TOOL_ID => self.remember(arguments),
            _ => anyhow::bail!("unknown notes tool `{name}`"),
        }
    }

    fn add(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<AddArgs>(NOTES_ADD_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let notes = NoteRepository::new(&store);
        let note = notes.add(agl_notes::NoteDraft::new(args.title, args.body))?;
        Ok(json!({
            "tool": NOTES_ADD_TOOL_ID,
            "note_id": note.id,
            "title": note.title,
            "status": "created",
        }))
    }

    fn search(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<SearchArgs>(NOTES_SEARCH_TOOL_ID, arguments)?;
        ensure!(
            !args.query.trim().is_empty(),
            "notes.search query cannot be blank"
        );
        let limit = args
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .min(MAX_SEARCH_LIMIT);
        let store = self.open_store_read_only()?;
        let notes = NoteRepository::new(&store);
        let results = notes.search(&NoteSearchQuery {
            text: Some(args.query),
            include_deleted: args.include_deleted.unwrap_or(false),
            limit,
        })?;
        let truncated = results.len() >= limit;
        let notes = results
            .into_iter()
            .map(|note| {
                json!({
                    "note_id": note.id,
                    "title": note.title,
                    "deleted": note.deleted_at.is_some(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tool": NOTES_SEARCH_TOOL_ID,
            "status": "ok",
            "match_count": notes.len(),
            "truncated": truncated,
            "notes": notes,
        }))
    }

    fn show(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<IdArgs>(NOTES_SHOW_TOOL_ID, arguments)?;
        let store = self.open_store_read_only()?;
        let notes = NoteRepository::new(&store);
        let note = notes
            .get(&args.id)?
            .with_context(|| format!("note not found: {}", args.id))?;
        let links = notes.links(&note.id)?;
        let links = links
            .into_iter()
            .map(|link| {
                json!({
                    "link_id": link.id,
                    "target_ref": link.target_ref,
                    "label": link.label,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tool": NOTES_SHOW_TOOL_ID,
            "status": "ok",
            "note_id": note.id,
            "title": note.title,
            "deleted": note.deleted_at.is_some(),
            "body_bytes": note.body.len(),
            "body": note.body,
            "links": links,
        }))
    }

    fn update(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<UpdateArgs>(NOTES_UPDATE_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let notes = NoteRepository::new(&store);
        let note = notes.update(
            &args.id,
            NoteUpdate {
                title: args.title,
                body: args.body,
            },
        )?;
        Ok(json!({
            "tool": NOTES_UPDATE_TOOL_ID,
            "note_id": note.id,
            "title": note.title,
            "status": "updated",
        }))
    }

    fn link(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<LinkArgs>(NOTES_LINK_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let notes = NoteRepository::new(&store);
        let link = notes.link(&args.id, &args.target_ref, args.label)?;
        Ok(json!({
            "tool": NOTES_LINK_TOOL_ID,
            "link_id": link.id,
            "note_id": link.note_id,
            "target_ref": link.target_ref,
            "status": "linked",
        }))
    }

    fn delete(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<IdArgs>(NOTES_DELETE_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let notes = NoteRepository::new(&store);
        let note = notes.delete(&args.id)?;
        Ok(json!({
            "tool": NOTES_DELETE_TOOL_ID,
            "note_id": note.id,
            "deleted": note.deleted_at.is_some(),
            "status": "deleted",
        }))
    }

    fn remember(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<RememberArgs>(NOTES_REMEMBER_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let notes = NoteRepository::new(&store);
        let scope = agl_memory::MemoryScope::new(
            parse_memory_scope_kind(args.scope.as_str())?,
            args.scope_key.unwrap_or_else(|| "default".to_string()),
        )?;
        let kind = parse_memory_kind(args.kind.as_str())?;
        let promotion = notes.remember(&args.id, scope, kind)?;
        Ok(json!({
            "tool": NOTES_REMEMBER_TOOL_ID,
            "note_id": promotion.note.id,
            "memory_id": promotion.memory.id,
            "link_id": promotion.link.id,
            "status": "remembered",
        }))
    }

    fn open_store_read_only(&self) -> Result<AglStore> {
        AglStore::open_current_read_only_at(&self.store_root)
            .with_context(|| format!("failed to open notes store {}", self.store_root.display()))
    }

    fn open_store_writable(&self) -> Result<AglStore> {
        AglStore::open_current_at(&self.store_root)
            .with_context(|| format!("failed to open notes store {}", self.store_root.display()))
    }
}

impl ActionHandler for NotesTools {
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
        ProviderId::new(PROVIDER_ID).expect("builtin notes provider id is valid"),
        "Notes Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin notes provider declaration is valid")
    .with_action(action::<AddArgs>(
        NOTES_ADD_TOOL_ID,
        "Create an explicit local note.",
        OperationKind::Write,
    ))
    .with_action(action::<SearchArgs>(
        NOTES_SEARCH_TOOL_ID,
        "Search local notes by title or body.",
        OperationKind::Read,
    ))
    .with_action(action::<IdArgs>(
        NOTES_SHOW_TOOL_ID,
        "Show one local note and its links.",
        OperationKind::Read,
    ))
    .with_action(action::<UpdateArgs>(
        NOTES_UPDATE_TOOL_ID,
        "Update one local note title or body.",
        OperationKind::Write,
    ))
    .with_action(action::<LinkArgs>(
        NOTES_LINK_TOOL_ID,
        "Link one local note to a memory, task, repo, Matrix, or URL reference.",
        OperationKind::Write,
    ))
    .with_action(action::<IdArgs>(
        NOTES_DELETE_TOOL_ID,
        "Tombstone one local note.",
        OperationKind::Write,
    ))
    .with_action(
        action::<RememberArgs>(
            NOTES_REMEMBER_TOOL_ID,
            "Promote one note into durable memory and link the note to the memory entry.",
            OperationKind::Approve,
        )
        .with_state_effects([StateEffect::StoreMemoryEntries, StateEffect::StoreNoteLinks]),
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
    let declaration = ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("builtin notes tool id is valid"),
        description,
        operation_kind,
    )
    .expect("builtin notes tool declaration schema is valid");
    match id {
        NOTES_ADD_TOOL_ID | NOTES_UPDATE_TOOL_ID => {
            declaration.with_state_effects([StateEffect::StoreNotes])
        }
        NOTES_DELETE_TOOL_ID => declaration.with_state_effects([StateEffect::StoreNotes]),
        NOTES_LINK_TOOL_ID => declaration.with_state_effects([StateEffect::StoreNoteLinks]),
        _ => declaration,
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    title: String,
    body: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<usize>,
    include_deleted: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    id: String,
    title: Option<String>,
    body: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LinkArgs {
    id: String,
    target_ref: String,
    label: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RememberArgs {
    id: String,
    scope: MemoryScopeArg,
    scope_key: Option<String>,
    kind: MemoryKindArg,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::migrated_temp_root;

    use super::*;

    #[test]
    fn notes_tools_add_search_show_and_link() {
        let root = migrated_temp_root("basic");
        let tools = NotesTools::new(&root);

        let add = tools
            .dispatch(
                NOTES_ADD_TOOL_ID,
                json!({"title":"Workflow","body":"Use pinned skills."}),
            )
            .unwrap();
        let note_id = add["note_id"].as_str().unwrap();
        let search = tools
            .dispatch(NOTES_SEARCH_TOOL_ID, json!({"query":"pinned"}))
            .unwrap();
        let show = tools
            .dispatch(NOTES_SHOW_TOOL_ID, json!({"id": note_id}))
            .unwrap();

        assert_eq!(search["match_count"], 1);
        assert_eq!(search["notes"][0]["title"], "Workflow");
        assert_eq!(show["body"], "Use pinned skills.");
        assert_eq!(show["links"], json!([]));
    }

    #[test]
    fn notes_tools_delete_and_remember_notes() {
        let root = migrated_temp_root("remember");
        let tools = NotesTools::new(&root);

        let add = tools
            .dispatch(
                NOTES_ADD_TOOL_ID,
                json!({"title":"Memory boundary","body":"Promote notes only explicitly."}),
            )
            .unwrap();
        let note_id = add["note_id"].as_str().unwrap();
        let remember = tools
            .dispatch(
                NOTES_REMEMBER_TOOL_ID,
                json!({
                    "id": note_id,
                    "scope": "user",
                    "kind": "decision"
                }),
            )
            .unwrap();

        assert_eq!(remember["status"], "remembered");
        assert!(remember["memory_id"].is_string());
        assert!(remember["link_id"].is_string());

        let delete = tools
            .dispatch(NOTES_DELETE_TOOL_ID, json!({"id": note_id}))
            .unwrap();
        let show = tools
            .dispatch(NOTES_SHOW_TOOL_ID, json!({"id": note_id}))
            .unwrap();

        assert_eq!(delete["status"], "deleted");
        assert_eq!(show["deleted"], true);
    }

    #[test]
    fn notes_declaration_registers_read_and_write_tools() {
        let declaration = declaration();
        declaration.validate().unwrap();
        assert!(
            declaration
                .action(&CapabilityId::new(NOTES_SEARCH_TOOL_ID).unwrap())
                .is_some()
        );
        let add = declaration
            .action(&CapabilityId::new(NOTES_ADD_TOOL_ID).unwrap())
            .unwrap();
        assert_eq!(add.input_schema["additionalProperties"], false);
        let schema = add.compile_schema().unwrap();
        assert!(
            schema
                .validate(&json!({"title": "Workflow", "body": "Use schemas."}))
                .is_ok()
        );
        assert!(schema.validate(&json!({"title": "Workflow"})).is_err());
        assert!(
            schema
                .validate(&json!({
                    "title": "Workflow",
                    "body": "Use schemas.",
                    "extra": true
                }))
                .is_err()
        );
        assert!(
            schema
                .validate(&json!({"title": 7, "body": "Use schemas."}))
                .is_err()
        );
    }
}
