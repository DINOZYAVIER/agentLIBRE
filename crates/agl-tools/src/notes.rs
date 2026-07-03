use std::path::{Path, PathBuf};

use agl_notes::{NoteRepository, NoteSearchQuery, NoteUpdate};
use agl_store::AglStore;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOperationKind, ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
    parse_tool_args as parse_args,
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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
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

    fn add(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<AddArgs>(NOTES_ADD_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let notes = NoteRepository::new(&store);
        let note = notes.add(agl_notes::NoteDraft::new(args.title, args.body))?;
        Ok(format!(
            "tool=notes.add\nnote_id={}\ntitle={}\nstatus=created",
            note.id, note.title
        ))
    }

    fn search(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<SearchArgs>(NOTES_SEARCH_TOOL_ID, arguments)?;
        ensure!(
            !args.query.trim().is_empty(),
            "notes.search query cannot be blank"
        );
        let limit = args
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .min(MAX_SEARCH_LIMIT);
        let store = self.open_store()?;
        let notes = NoteRepository::new(&store);
        let results = notes.search(&NoteSearchQuery {
            text: Some(args.query),
            include_deleted: args.include_deleted.unwrap_or(false),
            limit,
        })?;
        let mut output = format!(
            "tool=notes.search\nmatches={}\ntruncated={}\n---",
            results.len(),
            results.len() >= limit
        );
        for note in results {
            output.push('\n');
            output.push_str(&format!(
                "note id={} title={} deleted={}",
                note.id,
                note.title,
                note.deleted_at.is_some()
            ));
        }
        Ok(output)
    }

    fn show(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<IdArgs>(NOTES_SHOW_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let notes = NoteRepository::new(&store);
        let note = notes
            .get(&args.id)?
            .with_context(|| format!("note not found: {}", args.id))?;
        let links = notes.links(&note.id)?;
        let mut output = format!(
            "tool=notes.show\nnote_id={}\ntitle={}\ndeleted={}\nbody_bytes={}\n---\n{}",
            note.id,
            note.title,
            note.deleted_at.is_some(),
            note.body.len(),
            note.body
        );
        for link in links {
            output.push('\n');
            output.push_str(&format!(
                "link id={} target_ref={} label={}",
                link.id,
                link.target_ref,
                link.label.unwrap_or_default()
            ));
        }
        Ok(output)
    }

    fn update(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<UpdateArgs>(NOTES_UPDATE_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let notes = NoteRepository::new(&store);
        let note = notes.update(
            &args.id,
            NoteUpdate {
                title: args.title,
                body: args.body,
            },
        )?;
        Ok(format!(
            "tool=notes.update\nnote_id={}\ntitle={}\nstatus=updated",
            note.id, note.title
        ))
    }

    fn link(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<LinkArgs>(NOTES_LINK_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let notes = NoteRepository::new(&store);
        let link = notes.link(&args.id, &args.target_ref, args.label)?;
        Ok(format!(
            "tool=notes.link\nlink_id={}\nnote_id={}\ntarget_ref={}\nstatus=linked",
            link.id, link.note_id, link.target_ref
        ))
    }

    fn delete(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<IdArgs>(NOTES_DELETE_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let notes = NoteRepository::new(&store);
        let note = notes.delete(&args.id)?;
        Ok(format!(
            "tool=notes.delete\nnote_id={}\ndeleted={}\nstatus=deleted",
            note.id,
            note.deleted_at.is_some()
        ))
    }

    fn remember(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<RememberArgs>(NOTES_REMEMBER_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let notes = NoteRepository::new(&store);
        let scope = agl_memory::MemoryScope::new(
            parse_memory_scope_kind(&args.scope)?,
            args.scope_key.unwrap_or_else(|| "default".to_string()),
        )?;
        let kind = parse_memory_kind(&args.kind)?;
        let promotion = notes.remember(&args.id, scope, kind)?;
        Ok(format!(
            "tool=notes.remember\nnote_id={}\nmemory_id={}\nlink_id={}\nstatus=remembered",
            promotion.note.id, promotion.memory.id, promotion.link.id
        ))
    }

    fn open_store(&self) -> Result<AglStore> {
        AglStore::open_at(&self.store_root)
            .with_context(|| format!("failed to open notes store {}", self.store_root.display()))
    }
}

impl ToolHandler for NotesTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin notes provider id is valid"),
        "Notes Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin notes provider declaration is valid")
    .with_tool(tool(
        NOTES_ADD_TOOL_ID,
        "Create an explicit local note.",
        ToolCapability::Write,
        &["title", "body"],
    ))
    .with_tool(tool(
        NOTES_SEARCH_TOOL_ID,
        "Search local notes by title or body.",
        ToolCapability::Read,
        &["query"],
    ))
    .with_tool(tool(
        NOTES_SHOW_TOOL_ID,
        "Show one local note and its links.",
        ToolCapability::Read,
        &["id"],
    ))
    .with_tool(tool(
        NOTES_UPDATE_TOOL_ID,
        "Update one local note title or body.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(tool(
        NOTES_LINK_TOOL_ID,
        "Link one local note to a memory, task, repo, Matrix, or URL reference.",
        ToolCapability::Write,
        &["id", "target_ref"],
    ))
    .with_tool(tool(
        NOTES_DELETE_TOOL_ID,
        "Tombstone one local note.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(
        tool(
            NOTES_REMEMBER_TOOL_ID,
            "Promote one note into durable memory and link the note to the memory entry.",
            ToolCapability::Write,
            &["id", "scope", "kind"],
        )
        .with_operation_kind(ToolOperationKind::Approve),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn tool(
    id: &str,
    description: impl Into<String>,
    capability: ToolCapability,
    required_arguments: &[&str],
) -> ToolDeclaration {
    let declaration = ToolDeclaration::new(
        ToolId::new(id).expect("builtin notes tool id is valid"),
        description,
        capability,
        required_arguments.iter().copied(),
    );
    match id {
        NOTES_ADD_TOOL_ID | NOTES_UPDATE_TOOL_ID => {
            declaration.with_state_effects([ToolStateEffect::StoreNotes])
        }
        NOTES_DELETE_TOOL_ID => declaration.with_state_effects([ToolStateEffect::StoreNotes]),
        NOTES_LINK_TOOL_ID => declaration.with_state_effects([ToolStateEffect::StoreNoteLinks]),
        NOTES_REMEMBER_TOOL_ID => declaration.with_state_effects([
            ToolStateEffect::StoreMemoryEntries,
            ToolStateEffect::StoreNoteLinks,
        ]),
        _ => declaration,
    }
}

fn parse_memory_scope_kind(scope: &str) -> Result<agl_memory::MemoryScopeKind> {
    match scope {
        "user" => Ok(agl_memory::MemoryScopeKind::User),
        "repo" => Ok(agl_memory::MemoryScopeKind::Repo),
        "matrix_room" => Ok(agl_memory::MemoryScopeKind::MatrixRoom),
        "matrix_user" => Ok(agl_memory::MemoryScopeKind::MatrixUser),
        _ => anyhow::bail!("unknown memory scope `{scope}`"),
    }
}

fn parse_memory_kind(kind: &str) -> Result<agl_memory::MemoryKind> {
    match kind {
        "fact" => Ok(agl_memory::MemoryKind::Fact),
        "preference" => Ok(agl_memory::MemoryKind::Preference),
        "summary" => Ok(agl_memory::MemoryKind::Summary),
        "decision" => Ok(agl_memory::MemoryKind::Decision),
        "working_note" => Ok(agl_memory::MemoryKind::WorkingNote),
        _ => anyhow::bail!("unknown memory kind `{kind}`"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    title: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<usize>,
    include_deleted: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    id: String,
    title: Option<String>,
    body: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkArgs {
    id: String,
    target_ref: String,
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberArgs {
    id: String,
    scope: String,
    scope_key: Option<String>,
    kind: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    #[test]
    fn notes_tools_add_search_show_and_link() {
        let root = temp_root("basic");
        let tools = NotesTools::new(&root);

        let add = tools
            .dispatch(
                NOTES_ADD_TOOL_ID,
                json!({"title":"Workflow","body":"Use pinned skills."}),
            )
            .unwrap();
        let note_id = add
            .lines()
            .find_map(|line| line.strip_prefix("note_id="))
            .unwrap()
            .to_string();
        let search = tools
            .dispatch(NOTES_SEARCH_TOOL_ID, json!({"query":"pinned"}))
            .unwrap();
        let show = tools
            .dispatch(NOTES_SHOW_TOOL_ID, json!({"id": note_id}))
            .unwrap();

        assert!(search.contains("matches=1"));
        assert!(show.contains("Use pinned skills."));

        cleanup(root);
    }

    #[test]
    fn notes_tools_delete_and_remember_notes() {
        let root = temp_root("remember");
        let tools = NotesTools::new(&root);

        let add = tools
            .dispatch(
                NOTES_ADD_TOOL_ID,
                json!({"title":"Memory boundary","body":"Promote notes only explicitly."}),
            )
            .unwrap();
        let note_id = value_for(&add, "note_id=").unwrap();
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

        assert!(remember.contains("status=remembered"));
        assert!(remember.contains("memory_id="));
        assert!(remember.contains("link_id="));

        let delete = tools
            .dispatch(NOTES_DELETE_TOOL_ID, json!({"id": note_id}))
            .unwrap();
        let show = tools
            .dispatch(NOTES_SHOW_TOOL_ID, json!({"id": note_id}))
            .unwrap();

        assert!(delete.contains("status=deleted"));
        assert!(show.contains("deleted=true"));

        cleanup(root);
    }

    #[test]
    fn notes_declaration_registers_read_and_write_tools() {
        let mut catalog = ToolCatalog::new();
        register(&mut catalog).unwrap();

        assert!(
            catalog
                .tool(&ToolId::new(NOTES_SEARCH_TOOL_ID).unwrap())
                .is_some()
        );
        assert!(
            catalog
                .tool(&ToolId::new(NOTES_ADD_TOOL_ID).unwrap())
                .is_some()
        );
    }

    fn value_for(output: &str, prefix: &str) -> Option<String> {
        output
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::to_string)
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-notes-tools-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn cleanup(root: PathBuf) {
        let _ = std::fs::remove_dir_all(root);
    }
}
