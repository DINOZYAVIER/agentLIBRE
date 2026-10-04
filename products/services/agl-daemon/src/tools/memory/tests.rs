use super::*;
use crate::agent::memory_index::SearchHit;

fn page(text: &str) -> SearchPage {
    SearchPage {
        hits: vec![SearchHit {
            score: -1.0,
            slug: "runtime-build".into(),
            text: text.into(),
            sources: "message-1".into(),
            stale: false,
            superseded: false,
            supersession: None,
        }],
        total: 8,
        skipped: vec![],
        unchecked: vec![],
        fallback: String::new(),
        index_path: "forge:memory/INDEX.md".into(),
    }
}

#[test]
fn formatting_preserves_sources_status_and_verification_separately() {
    let mut page = page("Use a locked build.");
    page.hits[0].stale = true;
    page.hits[0].superseded = true;
    page.skipped.push("entries/bad.md".into());
    page.unchecked
        .push("forge:memory/entries/runtime-build.md".into());
    let text = fit_page(page, |text| Ok(text.len() <= 600)).unwrap();
    assert!(text.starts_with("1. [-1.000000] runtime-build — Use a locked build. [src: message-1] [status: stale, superseded]"));
    assert!(text.contains("Total claims: 8"));
    assert!(text.contains("Skipped files: entries/bad.md"));
    assert!(text.contains("Unchecked Forge files: forge:memory/entries/runtime-build.md"));
}

#[test]
fn byte_budget_drops_lower_ranks_then_truncates_one_at_word_boundary() {
    let original = "Long durable sourced fact ".repeat(100);
    let mut page = page(&original);
    page.hits.push(SearchHit {
        score: -0.5,
        slug: "second".into(),
        text: "lower ranked".into(),
        sources: "message-2".into(),
        stale: false,
        superseded: false,
        supersession: None,
    });
    let text = fit_page(page, |text| {
        assert!(text.contains("Total claims: 8"));
        Ok(text.len() <= 600)
    })
    .unwrap();
    assert!(text.len() <= 600);
    assert!(!text.contains("lower ranked"));
    assert!(text.contains(" […truncated] [src: message-1]"));
    let prefix = text
        .split(" — ")
        .nth(1)
        .unwrap()
        .split(" […truncated]")
        .next()
        .unwrap();
    assert!(original.starts_with(prefix));
    assert!(original[prefix.len()..].starts_with(char::is_whitespace));
}

#[test]
fn metadata_overflow_returns_exact_entry_action() {
    let mut page = page("fact");
    page.hits[0].sources = "source-id ".repeat(100);
    let failure = fit_page(page, |text| Ok(text.len() <= 600)).unwrap_err();
    assert_eq!(failure.kind, ToolFailureKind::ResultTooLarge);
    let text = serde_json::to_string(&failure).unwrap();
    assert!(text.contains("forge:memory/entries/runtime-build.md"));
}

#[test]
fn no_match_keeps_index_fallback_outside_the_hit_budget() {
    let mut page = page("unused");
    page.hits.clear();
    page.fallback = "INDEX content ".repeat(100);
    let text = fit_page(page, |_| panic!("INDEX fallback has a separate budget")).unwrap();
    assert!(text.contains("forge:memory/INDEX.md"));
    assert!(text.len() > 600);
}

#[test]
fn unicode_single_word_is_never_cut_inside_utf8_or_a_word() {
    let text = fit_page(page(&"世界".repeat(500)), |text| Ok(text.len() <= 600)).unwrap();
    assert!(text.contains("— […truncated] [src:"));
    assert!(text.len() <= 600);
}

#[test]
fn dropped_tool_future_cancels_the_blocking_tokenizer_request() {
    let cancellation = InferenceCancellation::new();
    let guard = CancelOnDrop(cancellation.clone());
    let future = async move {
        let _guard = guard;
        std::future::pending::<()>().await
    };
    drop(future);
    assert!(cancellation.is_cancelled());
}

#[test]
fn truncation_preserves_an_unresolved_supersession_tag() {
    let mut page = page(&"Long fact ".repeat(200));
    page.hits[0].supersession = Some("missing#claim-1".into());
    let text = fit_page(page, |text| Ok(text.len() <= 600)).unwrap();
    assert!(text.contains("[superseded-by: missing#claim-1]"));
    assert!(text.contains("[…truncated]"));
}
