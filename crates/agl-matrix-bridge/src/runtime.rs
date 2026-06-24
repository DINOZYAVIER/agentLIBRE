use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::EncryptionInfo;
use matrix_sdk::ruma::events::relation::Thread;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
    RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::{OwnedEventId, OwnedUserId};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{Client, Room, SessionMeta, SessionTokens};

use crate::{
    BridgeApp, BridgeConfig, BridgeInboundEvent, BridgeOutboundAction, EncryptionState,
    LazyDaemonClient, MatrixConfig,
};

pub struct MatrixRuntime {
    client: Client,
    app: Arc<Mutex<BridgeApp>>,
    socket_path: PathBuf,
    bot_user_id: String,
}

type MatrixMessageRelation = Relation<RoomMessageEventContentWithoutRelation>;

impl MatrixRuntime {
    pub async fn from_config(config: BridgeConfig, socket_path: PathBuf) -> Result<Self> {
        config
            .validate()
            .map_err(|err| anyhow!("bridge config is invalid: {err:?}"))?;
        let session = matrix_session_from_config(&config.matrix)?;
        let bot_user_id = config.matrix.user_id.clone();
        let client = Client::builder()
            .homeserver_url(config.matrix.homeserver_url.as_str())
            .build()
            .await
            .context("failed to build Matrix client")?;
        client
            .matrix_auth()
            .restore_session(session, RoomLoadSettings::default())
            .await
            .context("failed to restore Matrix access-token session")?;
        let app = Arc::new(Mutex::new(BridgeApp::from_config(config)?));

        Ok(Self {
            client,
            app,
            socket_path,
            bot_user_id,
        })
    }

    pub async fn sync_forever(&self) -> Result<()> {
        self.register_bridge_handler();
        self.client
            .sync(SyncSettings::default())
            .await
            .context("Matrix sync loop exited with error")
    }

    fn register_bridge_handler(&self) {
        let app = Arc::clone(&self.app);
        let socket_path = self.socket_path.clone();
        let bot_user_id = self.bot_user_id.clone();

        self.client.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent,
                  room: Room,
                  encryption_info: Option<EncryptionInfo>| {
                let app = Arc::clone(&app);
                let socket_path = socket_path.clone();
                let bot_user_id = bot_user_id.clone();

                async move {
                    if event.sender.as_str() == bot_user_id.as_str() {
                        return;
                    }
                    let Some((inbound, reply_context)) = inbound_event_from_original(
                        event,
                        room.room_id().to_string(),
                        encryption_info.is_some(),
                    ) else {
                        return;
                    };
                    match handle_inbound_on_blocking_thread(app, socket_path, inbound).await {
                        Ok(actions) => {
                            if let Err(error) =
                                send_outbound_actions(&room, &reply_context, &actions).await
                            {
                                tracing::warn!(
                                    error = %error,
                                    "failed to send Matrix bridge reply"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "Matrix bridge event handling failed"
                            );
                            let _ =
                                send_matrix_notice(&room, &reply_context, "Bridge command failed.")
                                    .await;
                        }
                    }
                }
            },
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MatrixReplyContext {
    thread_root_event_id: String,
    reply_event_id: String,
}

async fn handle_inbound_on_blocking_thread(
    app: Arc<Mutex<BridgeApp>>,
    socket_path: PathBuf,
    inbound: BridgeInboundEvent,
) -> Result<Vec<BridgeOutboundAction>> {
    tokio::task::spawn_blocking(move || {
        let mut app = app
            .lock()
            .map_err(|_| anyhow!("Matrix bridge app lock poisoned"))?;
        let mut client = LazyDaemonClient::new(socket_path);
        app.handle_event(inbound, &mut client)
    })
    .await
    .context("Matrix bridge worker task failed")?
}

fn inbound_event_from_original(
    event: OriginalSyncRoomMessageEvent,
    room_id: String,
    was_decrypted: bool,
) -> Option<(BridgeInboundEvent, MatrixReplyContext)> {
    let event_id = event.event_id.to_string();
    let thread_root_event_id = thread_root_event_id(&event.content).map(ToOwned::to_owned);
    let MessageType::Text(text) = event.content.msgtype else {
        return None;
    };
    let reply_context = MatrixReplyContext {
        thread_root_event_id: thread_root_event_id
            .clone()
            .unwrap_or_else(|| event_id.clone()),
        reply_event_id: event_id.clone(),
    };
    let encryption = if was_decrypted {
        EncryptionState::Decrypted
    } else {
        EncryptionState::Plaintext
    };

    Some((
        BridgeInboundEvent {
            event_id,
            room_id,
            sender_user_id: event.sender.to_string(),
            thread_root_event_id,
            body: text.body,
            encryption,
        },
        reply_context,
    ))
}

fn thread_root_event_id(content: &RoomMessageEventContent) -> Option<&str> {
    match &content.relates_to {
        Some(Relation::Thread(thread)) => Some(thread.event_id.as_str()),
        _ => None,
    }
}

async fn send_outbound_actions(
    room: &Room,
    context: &MatrixReplyContext,
    actions: &[BridgeOutboundAction],
) -> Result<()> {
    for action in actions {
        match action {
            BridgeOutboundAction::ReplyInThread { body } => {
                send_matrix_text(room, context, body).await?;
            }
            BridgeOutboundAction::NoticeInThread { body } => {
                send_matrix_notice(room, context, body).await?;
            }
            BridgeOutboundAction::Ignore { .. }
            | BridgeOutboundAction::MarkProcessed { .. }
            | BridgeOutboundAction::PersistBinding { .. }
            | BridgeOutboundAction::RemoveBinding { .. } => {}
        }
    }
    Ok(())
}

async fn send_matrix_text(room: &Room, context: &MatrixReplyContext, body: &str) -> Result<()> {
    let mut content = RoomMessageEventContent::text_plain(body);
    content.relates_to = Some(thread_relation(context)?);
    room.send(content)
        .await
        .context("Matrix room send failed")?;
    Ok(())
}

async fn send_matrix_notice(room: &Room, context: &MatrixReplyContext, body: &str) -> Result<()> {
    let mut content = RoomMessageEventContent::notice_plain(body);
    content.relates_to = Some(thread_relation(context)?);
    room.send(content)
        .await
        .context("Matrix room send failed")?;
    Ok(())
}

fn thread_relation(context: &MatrixReplyContext) -> Result<MatrixMessageRelation> {
    let root =
        OwnedEventId::try_from(context.thread_root_event_id.as_str()).with_context(|| {
            format!(
                "invalid Matrix thread root event id {}",
                context.thread_root_event_id
            )
        })?;
    let reply_to = OwnedEventId::try_from(context.reply_event_id.as_str())
        .with_context(|| format!("invalid Matrix reply event id {}", context.reply_event_id))?;
    Ok(Relation::Thread(Thread::plain(root, reply_to)))
}

fn matrix_session_from_config(config: &MatrixConfig) -> Result<MatrixSession> {
    let device_id = config
        .device_id
        .as_deref()
        .map(str::trim)
        .filter(|device_id| !device_id.is_empty())
        .context("matrix.device_id is required for access-token sync")?;
    let user_id = OwnedUserId::try_from(config.user_id.as_str())
        .with_context(|| format!("invalid Matrix user id {}", config.user_id))?;
    Ok(MatrixSession {
        meta: SessionMeta {
            user_id,
            device_id: device_id.into(),
        },
        tokens: SessionTokens {
            access_token: config.access_token.clone(),
            refresh_token: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix_config(device_id: Option<&str>) -> MatrixConfig {
        MatrixConfig {
            homeserver_url: "https://matrix.example".to_string(),
            user_id: "@agl:example".to_string(),
            access_token: "secret-token".to_string(),
            device_id: device_id.map(ToOwned::to_owned),
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
    fn thread_relation_rejects_invalid_event_ids() {
        let context = MatrixReplyContext {
            thread_root_event_id: "not-event".to_string(),
            reply_event_id: "$event:example".to_string(),
        };

        let error = thread_relation(&context).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid Matrix thread root event id")
        );
    }
}
