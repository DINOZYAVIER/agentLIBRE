use std::path::PathBuf;

use super::*;

#[test]
fn adds_and_reads_user_memory() {
    let root = temp_root("add-user");
    let store = AglStore::open_at(&root).unwrap();
    let repo = MemoryRepository::new(&store);

    let entry = repo
        .add(MemoryDraft::new(
            MemoryScope::user(),
            MemoryKind::Preference,
            "Commit style",
            "Use short imperative commit subjects.",
        ))
        .unwrap();

    assert_eq!(entry.scope, MemoryScope::user());
    assert_eq!(entry.kind, MemoryKind::Preference);
    assert_eq!(
        repo.get(&entry.id).unwrap().unwrap().body,
        "Use short imperative commit subjects."
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn search_is_scoped() {
    let root = temp_root("scoped-search");
    let store = AglStore::open_at(&root).unwrap();
    let repo = MemoryRepository::new(&store);
    let repo_scope = MemoryScope::new(MemoryScopeKind::Repo, "/tmp/repo-a").unwrap();
    repo.add(MemoryDraft::new(
        MemoryScope::user(),
        MemoryKind::Fact,
        "Matrix",
        "Matrix uses room scoped trust.",
    ))
    .unwrap();
    repo.add(MemoryDraft::new(
        repo_scope.clone(),
        MemoryKind::Decision,
        "Matrix",
        "Repo stores Matrix bridge fixtures.",
    ))
    .unwrap();

    let results = repo
        .search(&MemorySearchQuery::text(Some(repo_scope), "Matrix"))
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].scope.kind, MemoryScopeKind::Repo);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn deleted_memory_is_hidden_by_default() {
    let root = temp_root("delete");
    let store = AglStore::open_at(&root).unwrap();
    let repo = MemoryRepository::new(&store);
    let entry = repo
        .add(MemoryDraft::new(
            MemoryScope::user(),
            MemoryKind::Fact,
            "Temporary",
            "This should be tombstoned.",
        ))
        .unwrap();

    let deleted = repo.delete(&entry.id).unwrap();
    let hidden = repo
        .list(&MemorySearchQuery::scoped(MemoryScope::user()))
        .unwrap();
    let mut include_deleted = MemorySearchQuery::scoped(MemoryScope::user());
    include_deleted.include_deleted = true;
    let visible = repo.list(&include_deleted).unwrap();

    assert!(deleted.deleted_at.is_some());
    assert!(hidden.is_empty());
    assert_eq!(visible.len(), 1);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn suggestions_can_be_approved_into_memory() {
    let root = temp_root("suggest-approve");
    let store = AglStore::open_at(&root).unwrap();
    let repo = MemoryRepository::new(&store);

    let mut draft = MemorySuggestionDraft::new(
        MemoryScope::user(),
        MemoryKind::Decision,
        "Workflow",
        "Use pending memory suggestions.",
        "chat:turn-1",
    );
    draft.confidence = 88;
    let suggestion = repo.suggest(draft).unwrap();
    let pending = repo
        .list_suggestions(&MemorySuggestionQuery::pending(Some(MemoryScope::user())))
        .unwrap();
    let (approved, entry) = repo.approve_suggestion(&suggestion.id).unwrap();

    assert_eq!(pending, vec![suggestion]);
    assert_eq!(approved.status, MemorySuggestionStatus::Approved);
    let expected_ref = format!("memory:{}", entry.id);
    assert_eq!(
        approved.resolution_ref.as_deref(),
        Some(expected_ref.as_str())
    );
    assert_eq!(entry.kind, MemoryKind::Decision);
    assert_eq!(entry.confidence, 88);
    assert_eq!(entry.source_ref.as_deref(), Some("chat:turn-1"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn suggestions_can_be_rejected_without_memory() {
    let root = temp_root("suggest-reject");
    let store = AglStore::open_at(&root).unwrap();
    let repo = MemoryRepository::new(&store);
    let suggestion = repo
        .suggest(MemorySuggestionDraft::new(
            MemoryScope::user(),
            MemoryKind::Fact,
            "Noise",
            "Do not store this.",
            "chat:turn-2",
        ))
        .unwrap();

    let rejected = repo
        .reject_suggestion(&suggestion.id, Some("not durable"))
        .unwrap();
    let memories = repo
        .list(&MemorySearchQuery::scoped(MemoryScope::user()))
        .unwrap();

    assert_eq!(rejected.status, MemorySuggestionStatus::Rejected);
    assert_eq!(rejected.resolution_note.as_deref(), Some("not durable"));
    assert!(memories.is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_blank_memory_body() {
    let root = temp_root("blank-body");
    let store = AglStore::open_at(&root).unwrap();
    let repo = MemoryRepository::new(&store);

    let err = repo
        .add(MemoryDraft::new(
            MemoryScope::user(),
            MemoryKind::Fact,
            "Blank",
            " ",
        ))
        .unwrap_err();

    assert!(matches!(
        err,
        MemoryError::InvalidValue { field: "body", .. }
    ));

    std::fs::remove_dir_all(root).unwrap();
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agl-memory-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}
