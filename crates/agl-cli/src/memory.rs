use agl_memory::{
    MemoryDraft, MemoryEntry, MemoryKind, MemoryRepository, MemoryScope, MemoryScopeKind,
    MemorySearchQuery, MemorySuggestion, MemorySuggestionDraft, MemorySuggestionQuery,
    MemorySuggestionStatus,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::AglStore;
use anyhow::{Context, Result, bail};

use crate::args::{
    MemoryAddOptions, MemoryApproveOptions, MemoryCommand, MemoryDeleteOptions, MemoryKindArg,
    MemoryListOptions, MemoryListSuggestionsOptions, MemoryRejectOptions, MemoryScopeArg,
    MemorySearchOptions, MemoryShowOptions, MemorySuggestOptions, MemorySuggestionStatusArg,
};

pub(crate) fn run_memory(command: MemoryCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "memory", "starting command");
    let store =
        AglStore::open_at(runtime.paths.store_root()).context("failed to open memory store")?;
    let memory = MemoryRepository::new(&store);

    match command {
        MemoryCommand::Add(options) => run_memory_add(options, &memory),
        MemoryCommand::List(options) => run_memory_list(options, &memory),
        MemoryCommand::Search(options) => run_memory_search(options, &memory),
        MemoryCommand::Show(options) => run_memory_show(options, &memory),
        MemoryCommand::Delete(options) => run_memory_delete(options, &memory),
        MemoryCommand::Suggest(options) => run_memory_suggest(options, &memory),
        MemoryCommand::ListSuggestions(options) => run_memory_list_suggestions(options, &memory),
        MemoryCommand::Approve(options) => run_memory_approve(options, &memory),
        MemoryCommand::Reject(options) => run_memory_reject(options, &memory),
    }
}

fn run_memory_add(options: MemoryAddOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut draft = MemoryDraft::new(
        scope,
        memory_kind(options.kind),
        options.title,
        options.body,
    );
    draft.source_ref = options.source_ref;
    draft.confidence = options.confidence;
    let entry = memory.add(draft).context("failed to add memory entry")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
    } else {
        print_memory_entry_summary(&entry);
    }
    Ok(())
}

fn run_memory_list(options: MemoryListOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut query = MemorySearchQuery::scoped(scope);
    query.include_deleted = options.include_deleted;
    query.limit = options.limit;
    let entries = memory
        .list(&query)
        .context("failed to list memory entries")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_memory_entries(&entries);
    }
    Ok(())
}

fn run_memory_search(options: MemorySearchOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut query = MemorySearchQuery::text(Some(scope), options.query);
    query.include_deleted = options.include_deleted;
    query.limit = options.limit;
    let entries = memory
        .search(&query)
        .context("failed to search memory entries")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_memory_entries(&entries);
    }
    Ok(())
}

fn run_memory_show(options: MemoryShowOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let entry = memory
        .get(&options.id)
        .context("failed to read memory entry")?
        .ok_or_else(|| anyhow::anyhow!("memory entry not found: {}", options.id))?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
    } else {
        print_memory_entry_detail(&entry);
    }
    Ok(())
}

fn run_memory_delete(options: MemoryDeleteOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let entry = memory
        .delete(&options.id)
        .context("failed to delete memory entry")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&entry)?);
    } else {
        println!("memory.deleted=true");
        print_memory_entry_summary(&entry);
    }
    Ok(())
}

fn run_memory_suggest(options: MemorySuggestOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let scope = memory_scope(options.scope, options.scope_key)?;
    let mut draft = MemorySuggestionDraft::new(
        scope,
        memory_kind(options.kind),
        options.title,
        options.body,
        options.source_ref,
    );
    draft.confidence = options.confidence;
    let suggestion = memory
        .suggest(draft)
        .context("failed to create memory suggestion")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&suggestion)?);
    } else {
        print_memory_suggestion_summary(&suggestion);
    }
    Ok(())
}

fn run_memory_list_suggestions(
    options: MemoryListSuggestionsOptions,
    memory: &MemoryRepository<'_>,
) -> Result<()> {
    let scope = if options.all_scopes {
        None
    } else {
        Some(memory_scope(options.scope, options.scope_key)?)
    };
    let status = options
        .status
        .map(memory_suggestion_status)
        .or(Some(MemorySuggestionStatus::Pending));
    let suggestions = memory
        .list_suggestions(&MemorySuggestionQuery {
            scope,
            status,
            limit: options.limit,
        })
        .context("failed to list memory suggestions")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&suggestions)?);
    } else {
        print_memory_suggestions(&suggestions);
    }
    Ok(())
}

fn run_memory_approve(options: MemoryApproveOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let (suggestion, entry) = memory
        .approve_suggestion(&options.id)
        .context("failed to approve memory suggestion")?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "suggestion": suggestion,
                "memory": entry,
            }))?
        );
    } else {
        println!("memory_suggestion.approved=true");
        print_memory_suggestion_summary(&suggestion);
        print_memory_entry_summary(&entry);
    }
    Ok(())
}

fn run_memory_reject(options: MemoryRejectOptions, memory: &MemoryRepository<'_>) -> Result<()> {
    let suggestion = memory
        .reject_suggestion(&options.id, options.reason.as_deref())
        .context("failed to reject memory suggestion")?;

    if options.json {
        println!("{}", serde_json::to_string_pretty(&suggestion)?);
    } else {
        println!("memory_suggestion.rejected=true");
        print_memory_suggestion_summary(&suggestion);
    }
    Ok(())
}

pub(crate) fn memory_scope(kind: MemoryScopeArg, key: Option<String>) -> Result<MemoryScope> {
    let kind = match kind {
        MemoryScopeArg::User => MemoryScopeKind::User,
        MemoryScopeArg::Repo => MemoryScopeKind::Repo,
        MemoryScopeArg::MatrixRoom => MemoryScopeKind::MatrixRoom,
        MemoryScopeArg::MatrixUser => MemoryScopeKind::MatrixUser,
    };
    match (kind, key) {
        (MemoryScopeKind::User, None) => Ok(MemoryScope::user()),
        (kind, Some(key)) => MemoryScope::new(kind, key).map_err(anyhow::Error::from),
        (kind, None) => bail!("--scope-key is required for --scope {}", kind.as_str()),
    }
}

fn memory_suggestion_status(status: MemorySuggestionStatusArg) -> MemorySuggestionStatus {
    match status {
        MemorySuggestionStatusArg::Pending => MemorySuggestionStatus::Pending,
        MemorySuggestionStatusArg::Approved => MemorySuggestionStatus::Approved,
        MemorySuggestionStatusArg::Rejected => MemorySuggestionStatus::Rejected,
    }
}

pub(crate) fn memory_kind(kind: MemoryKindArg) -> MemoryKind {
    match kind {
        MemoryKindArg::Fact => MemoryKind::Fact,
        MemoryKindArg::Preference => MemoryKind::Preference,
        MemoryKindArg::Summary => MemoryKind::Summary,
        MemoryKindArg::Decision => MemoryKind::Decision,
        MemoryKindArg::WorkingNote => MemoryKind::WorkingNote,
    }
}

fn print_memory_entries(entries: &[MemoryEntry]) {
    for entry in entries {
        print_memory_entry_summary(entry);
    }
}

pub(crate) fn print_memory_entry_summary(entry: &MemoryEntry) {
    println!(
        "memory id={} scope={} scope_key={} kind={} title={} deleted={}",
        entry.id,
        entry.scope.kind.as_str(),
        entry.scope.key,
        entry.kind.as_str(),
        entry.title,
        entry.deleted_at.is_some()
    );
}

fn print_memory_entry_detail(entry: &MemoryEntry) {
    print_memory_entry_summary(entry);
    println!("memory.{}.confidence={}", entry.id, entry.confidence);
    println!("memory.{}.created_at={}", entry.id, entry.created_at);
    println!("memory.{}.updated_at={}", entry.id, entry.updated_at);
    if let Some(source_ref) = &entry.source_ref {
        println!("memory.{}.source_ref={source_ref}", entry.id);
    }
    if let Some(deleted_at) = &entry.deleted_at {
        println!("memory.{}.deleted_at={deleted_at}", entry.id);
    }
    println!("memory.{}.body={}", entry.id, entry.body);
}

fn print_memory_suggestions(suggestions: &[MemorySuggestion]) {
    for suggestion in suggestions {
        print_memory_suggestion_summary(suggestion);
    }
}

fn print_memory_suggestion_summary(suggestion: &MemorySuggestion) {
    println!(
        "memory_suggestion id={} scope={} scope_key={} kind={} status={} title={}",
        suggestion.id,
        suggestion.scope.kind.as_str(),
        suggestion.scope.key,
        suggestion.kind.as_str(),
        suggestion.status.as_str(),
        suggestion.title
    );
    println!(
        "memory_suggestion.{}.source_ref={}",
        suggestion.id, suggestion.source_ref
    );
    if let Some(resolution_ref) = &suggestion.resolution_ref {
        println!(
            "memory_suggestion.{}.resolution_ref={resolution_ref}",
            suggestion.id
        );
    }
    if let Some(resolution_note) = &suggestion.resolution_note {
        println!(
            "memory_suggestion.{}.resolution_note={resolution_note}",
            suggestion.id
        );
    }
}
