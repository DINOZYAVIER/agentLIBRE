use agl_notes::{Note, NoteDraft, NoteLink, NoteRepository, NoteSearchQuery, NoteUpdate};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::AglStore;
use anyhow::{Context, Result};

use crate::args::{
    NotesAddOptions, NotesCommand, NotesDeleteOptions, NotesLinkOptions, NotesListOptions,
    NotesRememberOptions, NotesSearchOptions, NotesShowOptions, NotesUpdateOptions,
};
use crate::memory::{memory_kind, memory_scope, print_memory_entry_summary};

pub(crate) fn run_notes(command: NotesCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "notes", "starting command");
    let store =
        AglStore::open_at(runtime.paths.store_root()).context("failed to open notes store")?;
    let notes = NoteRepository::new(&store);

    match command {
        NotesCommand::Add(options) => run_notes_add(options, &notes),
        NotesCommand::List(options) => run_notes_list(options, &notes),
        NotesCommand::Search(options) => run_notes_search(options, &notes),
        NotesCommand::Show(options) => run_notes_show(options, &notes),
        NotesCommand::Update(options) => run_notes_update(options, &notes),
        NotesCommand::Delete(options) => run_notes_delete(options, &notes),
        NotesCommand::Link(options) => run_notes_link(options, &notes),
        NotesCommand::Remember(options) => run_notes_remember(options, &notes),
    }
}
fn run_notes_add(options: NotesAddOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes
        .add(NoteDraft::new(options.title, options.body))
        .context("failed to add note")?;
    if options.json {
        crate::print_json(&note)?;
    } else {
        print_note_summary(&note);
    }
    Ok(())
}

fn run_notes_list(options: NotesListOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let query = NoteSearchQuery {
        include_deleted: options.include_deleted,
        limit: options.limit,
        ..NoteSearchQuery::default()
    };
    let entries = notes.list(&query).context("failed to list notes")?;
    if options.json {
        crate::print_json(&entries)?;
    } else {
        print_notes(&entries);
    }
    Ok(())
}

fn run_notes_search(options: NotesSearchOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let query = NoteSearchQuery {
        text: Some(options.query),
        include_deleted: options.include_deleted,
        limit: options.limit,
    };
    let entries = notes.search(&query).context("failed to search notes")?;
    if options.json {
        crate::print_json(&entries)?;
    } else {
        print_notes(&entries);
    }
    Ok(())
}

fn run_notes_show(options: NotesShowOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes
        .get(&options.id)
        .context("failed to read note")?
        .ok_or_else(|| anyhow::anyhow!("note not found: {}", options.id))?;
    let links = notes
        .links(&options.id)
        .context("failed to read note links")?;
    if options.json {
        crate::print_json(&serde_json::json!({
            "note": note,
            "links": links,
        }))?;
    } else {
        print_note_detail(&note, &links);
    }
    Ok(())
}

fn run_notes_update(options: NotesUpdateOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes
        .update(
            &options.id,
            NoteUpdate {
                title: options.title,
                body: options.body,
            },
        )
        .context("failed to update note")?;
    if options.json {
        crate::print_json(&note)?;
    } else {
        print_note_summary(&note);
    }
    Ok(())
}

fn run_notes_delete(options: NotesDeleteOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let note = notes.delete(&options.id).context("failed to delete note")?;
    if options.json {
        crate::print_json(&note)?;
    } else {
        println!("note.deleted=true");
        print_note_summary(&note);
    }
    Ok(())
}

fn run_notes_link(options: NotesLinkOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let link = notes
        .link(&options.id, &options.target_ref, options.label)
        .context("failed to link note")?;
    if options.json {
        crate::print_json(&link)?;
    } else {
        print_note_link(&link);
    }
    Ok(())
}

fn run_notes_remember(options: NotesRememberOptions, notes: &NoteRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let promotion = notes
        .remember(&options.id, scope, memory_kind(options.kind))
        .context("failed to promote note into memory")?;

    if options.json {
        crate::print_json(&serde_json::json!({
            "note": promotion.note,
            "memory": promotion.memory,
            "link": promotion.link,
        }))?;
    } else {
        println!("note.remembered=true");
        print_note_summary(&promotion.note);
        print_memory_entry_summary(&promotion.memory);
        print_note_link(&promotion.link);
    }
    Ok(())
}
fn print_notes(notes: &[Note]) {
    for note in notes {
        print_note_summary(note);
    }
}

fn print_note_summary(note: &Note) {
    println!(
        "note id={} title={} deleted={}",
        note.id,
        note.title,
        note.deleted_at.is_some()
    );
}

fn print_note_detail(note: &Note, links: &[NoteLink]) {
    print_note_summary(note);
    if note.deleted_at.is_some() {
        println!("note.{}.audit=tombstoned", note.id);
    }
    println!("note.{}.created_at={}", note.id, note.created_at);
    println!("note.{}.updated_at={}", note.id, note.updated_at);
    if let Some(deleted_at) = &note.deleted_at {
        println!("note.{}.deleted_at={deleted_at}", note.id);
    }
    println!("note.{}.body={}", note.id, note.body);
    for link in links {
        print_note_link(link);
    }
}

fn print_note_link(link: &NoteLink) {
    println!(
        "note_link id={} note_id={} target_ref={}",
        link.id, link.note_id, link.target_ref
    );
    if let Some(label) = &link.label {
        println!("note_link.{}.label={label}", link.id);
    }
}
