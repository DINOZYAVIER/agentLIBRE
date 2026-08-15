use super::*;

#[test]
fn validates_memory_without_sqlite() {
    let mut draft = MemoryDraft::new(MemoryScope::user(), MemoryKind::Fact, "title", "body");
    assert!(validate_memory_draft(&draft).is_ok());

    draft.title = "  ".to_owned();
    assert!(matches!(
        validate_memory_draft(&draft),
        Err(MemoryError::InvalidValue { field: "title", .. })
    ));
}

#[test]
fn validates_suggestion_without_sqlite() {
    let mut draft = MemorySuggestionDraft::new(
        MemoryScope::user(),
        MemoryKind::Preference,
        "title",
        "body",
        "run:1",
    );
    assert!(validate_memory_suggestion_draft(&draft).is_ok());

    draft.source_ref.clear();
    assert!(matches!(
        validate_memory_suggestion_draft(&draft),
        Err(MemoryError::InvalidValue {
            field: "source_ref",
            ..
        })
    ));
}
