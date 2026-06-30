use super::*;

fn matrix_config(device_id: Option<&str>) -> MatrixConfig {
    MatrixConfig {
        homeserver_url: "https://matrix.example".to_string(),
        user_id: "@agl:example".to_string(),
        access_token: Some("secret-token".to_string()),
        device_id: device_id.map(ToOwned::to_owned),
        session_path: None,
        store_path: None,
        command_prefix: "!agl".to_string(),
        normal_chat: false,
        encrypted_rooms: crate::EncryptedRoomPolicy::Reject,
    }
}

#[test]
fn access_token_session_requires_device_id() {
    let error = matrix_session_from_config(&matrix_config(None)).unwrap_err();

    assert!(error.to_string().contains("matrix.device_id is required"));
}

#[test]
fn access_token_session_validates_user_id() {
    let mut config = matrix_config(Some("DEVICE"));
    config.user_id = "not-a-user-id".to_string();

    let error = matrix_session_from_config(&config).unwrap_err();

    assert!(error.to_string().contains("invalid Matrix user id"));
}

#[test]
fn access_token_session_uses_config_identity() {
    let session = matrix_session_from_config(&matrix_config(Some("DEVICE"))).unwrap();

    assert_eq!(session.meta.user_id.as_str(), "@agl:example");
    assert_eq!(session.meta.device_id.as_str(), "DEVICE");
    assert_eq!(session.tokens.access_token, "secret-token");
}

#[test]
fn session_file_is_preferred_over_inline_token() {
    let path = std::env::temp_dir().join(format!(
        "agl-matrix-session-{}-preferred.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut saved = matrix_config(Some("DEVICE"));
    saved.access_token = Some("session-token".to_string());
    let session = matrix_session_from_config(&saved).unwrap();
    save_matrix_session(&path, &session).unwrap();
    let mut config = matrix_config(None);
    config.access_token = None;
    config.session_path = Some(path.display().to_string());

    let loaded = matrix_session_from_config(&config).unwrap();

    assert_eq!(loaded.meta.device_id.as_str(), "DEVICE");
    assert_eq!(loaded.tokens.access_token, "session-token");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn matrix_store_path_ignores_blank_values() {
    let mut config = matrix_config(Some("DEVICE"));
    config.store_path = Some("  ".to_string());

    assert_eq!(matrix_store_path(&config), None);

    config.store_path = Some("/tmp/agl-matrix-store".to_string());
    assert_eq!(
        matrix_store_path(&config),
        Some(PathBuf::from("/tmp/agl-matrix-store"))
    );
}

#[test]
fn message_relation_replies_to_room_event() {
    let context = MatrixReplyContext {
        thread_root_event_id: None,
        reply_event_id: "$event:example".to_string(),
    };

    match message_relation(&context).unwrap() {
        Relation::Reply { in_reply_to } => {
            assert_eq!(in_reply_to.event_id.as_str(), "$event:example");
        }
        relation => panic!("expected reply relation, got {relation:?}"),
    }
}

#[test]
fn message_relation_replies_inside_existing_thread() {
    let context = MatrixReplyContext {
        thread_root_event_id: Some("$thread:example".to_string()),
        reply_event_id: "$event:example".to_string(),
    };

    match message_relation(&context).unwrap() {
        Relation::Thread(thread) => {
            assert_eq!(thread.event_id.as_str(), "$thread:example");
            assert_eq!(
                thread.in_reply_to.unwrap().event_id.as_str(),
                "$event:example"
            );
            assert!(!thread.is_falling_back);
        }
        relation => panic!("expected thread relation, got {relation:?}"),
    }
}

#[test]
fn message_relation_rejects_invalid_thread_event_ids() {
    let context = MatrixReplyContext {
        thread_root_event_id: Some("not-event".to_string()),
        reply_event_id: "$event:example".to_string(),
    };

    let error = message_relation(&context).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("invalid Matrix thread root event id")
    );
}
