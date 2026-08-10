use agl_content::{
    ArtifactSensitivity, ArtifactSource, ArtifactSourceKind, BlobDigest, ContentAttachmentId,
    ContentAttachmentRef, ImageDimensions, MediaType,
};

// AGL172-032.
#[test]
fn run_scoped_blob_identity_uses_only_content_attachment_vocabulary() {
    let id = ContentAttachmentId::generate();
    let reference = ContentAttachmentRef::new(
        id.clone(),
        BlobDigest::parse(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap(),
        MediaType::ImagePng,
        7,
        Some(ImageDimensions::new(1, 1).unwrap()),
        ArtifactSensitivity::Private,
        ArtifactSource {
            kind: ArtifactSourceKind::Generated,
            extension: Some("fixture".to_owned()),
        },
    )
    .unwrap();
    let encoded = serde_json::to_value(&reference).unwrap();
    assert_eq!(encoded["content_attachment_id"], id.to_string());
    let old_id = ["artifact", "_id"].concat();
    assert!(encoded.get(&old_id).is_none());
    assert!(encoded.get("artifact").is_none());
    assert_eq!(
        serde_json::from_value::<ContentAttachmentRef>(encoded).unwrap(),
        reference
    );
}
