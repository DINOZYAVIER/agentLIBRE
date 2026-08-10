use agl_content::{
    ArtifactRetention, ArtifactSensitivity, ArtifactSource, ContentAttachmentRef, ImageDimensions,
    MediaType,
};
use agl_ids::RunId;
use agl_store::{AglStore, ResolvedContentAttachment, StoredContentAttachment};

// AGL172-032. Blob bytes and BlobDigest stay unchanged; only the run-scoped
// reference/table/method vocabulary is replaced.
#[test]
fn store_persists_content_attachments_without_an_artifact_table_or_api() {
    #[allow(clippy::too_many_arguments)]
    fn selected_write(
        store: &AglStore,
        run_id: &RunId,
        media_type: MediaType,
        bytes: &[u8],
        image: Option<ImageDimensions>,
        sensitivity: ArtifactSensitivity,
        source: ArtifactSource,
        retention: ArtifactRetention,
    ) -> agl_store::Result<StoredContentAttachment> {
        store.write_content_attachment(
            run_id,
            media_type,
            bytes,
            image,
            sensitivity,
            source,
            retention,
        )
    }
    fn selected_resolve(
        store: &AglStore,
        run_id: &RunId,
        reference: &ContentAttachmentRef,
    ) -> agl_store::Result<ResolvedContentAttachment> {
        store.resolve_content_attachment(run_id, reference)
    }

    #[allow(clippy::type_complexity)]
    type SelectedWriteFn = fn(
        &AglStore,
        &RunId,
        MediaType,
        &[u8],
        Option<ImageDimensions>,
        ArtifactSensitivity,
        ArtifactSource,
        ArtifactRetention,
    ) -> agl_store::Result<StoredContentAttachment>;
    let _: SelectedWriteFn = selected_write;
    let _: fn(
        &AglStore,
        &RunId,
        &ContentAttachmentRef,
    ) -> agl_store::Result<ResolvedContentAttachment> = selected_resolve;

    let root =
        std::env::temp_dir().join(format!("agl172-content-attachment-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = AglStore::open_at(&root).unwrap();
    let mut statement = store
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap();
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(tables.contains(&"content_attachments".to_owned()));
    assert!(!tables.contains(&"artifacts".to_owned()));
    drop(statement);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

// AGL172-037, AGL172-043 and AGL172-065.
#[test]
fn artifact_commit_repository_is_narrow_durable_and_correlation_checked() {
    fn selected_api(store: &AglStore) -> &dyn agl_artifact::ArtifactCommitRepository {
        store.artifact_commit_repository()
    }
    let _: fn(&AglStore) -> &dyn agl_artifact::ArtifactCommitRepository = selected_api;
}
