use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agl_store::{AglStore, MatrixNotificationOutboxItem};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::EncryptionInfo;
use matrix_sdk::encryption::verification::{
    EmojiShortAuthString, SasState, SasVerification, VerificationRequest, VerificationRequestState,
};
use matrix_sdk::ruma::events::key::verification::{
    VerificationMethod, request::ToDeviceKeyVerificationRequestEvent,
};
use matrix_sdk::ruma::events::relation::{Reply, Thread};
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
    RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::{OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedUserId};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{Client, ClientBuilder, Room, SessionMeta, SessionTokens};

use crate::{
    BridgeApp, BridgeConfig, BridgeInboundEvent, BridgeOutboundAction, EncryptionState,
    LazyDaemonClient, MatrixConfig, parse_matrix_room_notify_ref,
};

pub struct MatrixRuntime {
    client: Client,
    app: Arc<Mutex<BridgeApp>>,
    socket_path: PathBuf,
    bot_user_id: String,
}

type MatrixMessageRelation = Relation<RoomMessageEventContentWithoutRelation>;

pub struct MatrixPasswordLogin {
    pub username: String,
    pub password: String,
    pub device_display_name: String,
}

pub struct MatrixLoginResult {
    pub user_id: String,
    pub device_id: String,
    pub session_path: PathBuf,
    pub store_path: Option<PathBuf>,
}

pub struct MatrixDeviceVerificationRequest {
    pub user_id: String,
    pub device_id: String,
    pub timeout: Duration,
    pub accept_incoming: bool,
}

pub struct MatrixDeviceVerificationResult {
    pub user_id: String,
    pub device_id: String,
    pub flow_id: Option<String>,
    pub status: MatrixDeviceVerificationStatus,
}

pub struct MatrixUserDevice {
    pub user_id: String,
    pub device_id: String,
    pub display_name: Option<String>,
    pub verified: bool,
}

pub struct MatrixOutboxDeliveryReport {
    pub queued: usize,
    pub sent: usize,
    pub failed: usize,
    pub truncated: bool,
    pub items: Vec<MatrixOutboxDeliveryResult>,
}

pub struct MatrixOutboxDeliveryResult {
    pub id: String,
    pub notify_ref: String,
    pub action: MatrixOutboxDeliveryAction,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatrixOutboxDeliveryAction {
    Sent,
    Failed,
}

impl MatrixOutboxDeliveryAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatrixDeviceVerificationStatus {
    AlreadyVerified,
    Verified,
}

impl MatrixDeviceVerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyVerified => "already_verified",
            Self::Verified => "verified",
        }
    }
}

pub struct MatrixSasPresentation {
    pub flow_id: String,
    pub user_id: String,
    pub device_id: String,
    pub emojis: Vec<MatrixSasEmoji>,
    pub decimals: Option<(u16, u16, u16)>,
}

pub struct MatrixSasEmoji {
    pub symbol: String,
    pub description: String,
}

pub const ENV_MATRIX_USERNAME: &str = "AGL_MATRIX_USERNAME";
pub const ENV_MATRIX_PASSWORD: &str = "AGL_MATRIX_PASSWORD";
pub const ENV_MATRIX_DEVICE_DISPLAY_NAME: &str = "AGL_MATRIX_DEVICE_DISPLAY_NAME";

impl MatrixPasswordLogin {
    pub fn from_env() -> Result<Self> {
        let username = required_env(ENV_MATRIX_USERNAME, "password login")?;
        let password = required_env(ENV_MATRIX_PASSWORD, "password login")?;
        let device_display_name = std::env::var(ENV_MATRIX_DEVICE_DISPLAY_NAME)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "agl-matrix-bridge".to_string());

        Ok(Self {
            username,
            password,
            device_display_name,
        })
    }
}

fn required_env(name: &str, purpose: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{name} is required for {purpose}"))
}

impl MatrixRuntime {
    pub async fn from_config(config: BridgeConfig, socket_path: PathBuf) -> Result<Self> {
        config
            .validate()
            .map_err(|err| anyhow!("bridge config is invalid: {err:?}"))?;
        let bot_user_id = config.matrix.user_id.clone();
        let client = restore_matrix_client_from_config(&config.matrix).await?;
        let app = Arc::new(Mutex::new(BridgeApp::from_config(config)?));

        Ok(Self {
            client,
            app,
            socket_path,
            bot_user_id,
        })
    }

    pub async fn login_with_password(
        config: BridgeConfig,
        login: MatrixPasswordLogin,
        replace_session: bool,
    ) -> Result<MatrixLoginResult> {
        config
            .validate()
            .map_err(|err| anyhow!("bridge config is invalid: {err:?}"))?;
        let session_path = matrix_session_path(&config.matrix)?;
        let store_path = matrix_store_path(&config.matrix);
        validate_password_login_paths(&session_path, store_path.as_deref(), replace_session)?;
        let client = build_matrix_auth_client(&config.matrix).await?;
        let response = client
            .matrix_auth()
            .login_username(&login.username, &login.password)
            .initial_device_display_name(&login.device_display_name)
            .send()
            .await
            .context("Matrix password login failed")?;
        let session: MatrixSession = (&response).into();
        validate_session_user(&session, &config.matrix.user_id)?;
        save_matrix_session(&session_path, &session)?;

        Ok(MatrixLoginResult {
            user_id: session.meta.user_id.to_string(),
            device_id: session.meta.device_id.to_string(),
            session_path,
            store_path,
        })
    }

    pub async fn sync_forever(&self) -> Result<()> {
        self.register_bridge_handler();
        self.client
            .sync(SyncSettings::default())
            .await
            .context("Matrix sync loop exited with error")
    }

    pub async fn verify_device<F>(
        config: BridgeConfig,
        request: MatrixDeviceVerificationRequest,
        mut confirm_sas: F,
    ) -> Result<MatrixDeviceVerificationResult>
    where
        F: FnMut(&MatrixSasPresentation) -> Result<bool>,
    {
        config
            .validate()
            .map_err(|err| anyhow!("bridge config is invalid: {err:?}"))?;
        if request.timeout.is_zero() {
            bail!("Matrix device verification timeout must be greater than zero");
        }
        let _store_path = matrix_store_path(&config.matrix)
            .context("matrix.store_path is required for Matrix device verification")?;
        let target_user_id = OwnedUserId::try_from(request.user_id.as_str())
            .with_context(|| format!("invalid Matrix target user id {}", request.user_id))?;
        let target_device_id = OwnedDeviceId::from(request.device_id.as_str());
        let session = matrix_session_from_config(&config.matrix)?;
        if target_user_id != session.meta.user_id {
            bail!(
                "Matrix device verification only supports self-verification for the restored bridge session: target user {} does not match session user {}",
                target_user_id,
                session.meta.user_id
            );
        }
        if target_device_id == session.meta.device_id {
            bail!(
                "Matrix trusted device {} must be different from the bridge session device",
                target_device_id
            );
        }
        let client = restore_matrix_client(&config.matrix, session).await?;

        if request.accept_incoming {
            accept_incoming_device_verification(
                &client,
                target_user_id,
                target_device_id,
                request.timeout,
                &mut confirm_sas,
            )
            .await
        } else {
            verify_device_with_client(
                &client,
                target_user_id,
                target_device_id,
                request.timeout,
                &mut confirm_sas,
            )
            .await
        }
    }

    pub async fn list_user_devices(
        config: BridgeConfig,
        user_id: String,
    ) -> Result<Vec<MatrixUserDevice>> {
        config
            .validate()
            .map_err(|err| anyhow!("bridge config is invalid: {err:?}"))?;
        let target_user_id = OwnedUserId::try_from(user_id.as_str())
            .with_context(|| format!("invalid Matrix target user id {user_id}"))?;
        let client = restore_matrix_client_from_config(&config.matrix).await?;
        client
            .encryption()
            .request_user_identity(&target_user_id)
            .await
            .with_context(|| format!("failed to query Matrix identity for {}", target_user_id))?;
        let devices = client
            .encryption()
            .get_user_devices(&target_user_id)
            .await
            .with_context(|| format!("failed to read Matrix devices for {}", target_user_id))?;

        let mut devices = devices
            .devices()
            .map(|device| MatrixUserDevice {
                user_id: device.user_id().to_string(),
                device_id: device.device_id().to_string(),
                display_name: device.display_name().map(ToOwned::to_owned),
                verified: device.is_verified(),
            })
            .collect::<Vec<_>>();
        devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
        Ok(devices)
    }

    pub async fn deliver_outbox(
        config: BridgeConfig,
        store_root: impl AsRef<Path>,
        limit: usize,
    ) -> Result<MatrixOutboxDeliveryReport> {
        config
            .validate()
            .map_err(|err| anyhow!("bridge config is invalid: {err:?}"))?;
        let client = restore_matrix_client_from_config(&config.matrix).await?;
        client
            .sync_once(SyncSettings::default())
            .await
            .context("failed to sync Matrix room state before outbox delivery")?;

        let store = AglStore::open_current_at(store_root.as_ref())?;
        let limit = limit.max(1);
        let (queued, truncated) = store.queued_matrix_notifications_page(limit)?;
        let mut report = MatrixOutboxDeliveryReport {
            queued: queued.len(),
            sent: 0,
            failed: 0,
            truncated,
            items: Vec::with_capacity(queued.len()),
        };
        for item in queued {
            match deliver_matrix_notification(&client, &item).await {
                Ok(()) => {
                    let item = store.mark_matrix_notification_sent(&item.id)?;
                    report.sent += 1;
                    report.items.push(MatrixOutboxDeliveryResult {
                        id: item.id,
                        notify_ref: item.notify_ref,
                        action: MatrixOutboxDeliveryAction::Sent,
                        error: None,
                    });
                }
                Err(err) => {
                    let error = err.to_string();
                    let item = store.mark_matrix_notification_failed(&item.id, &error)?;
                    report.failed += 1;
                    report.items.push(MatrixOutboxDeliveryResult {
                        id: item.id,
                        notify_ref: item.notify_ref,
                        action: MatrixOutboxDeliveryAction::Failed,
                        error: Some(error),
                    });
                }
            }
        }
        Ok(report)
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

async fn deliver_matrix_notification(
    client: &Client,
    item: &MatrixNotificationOutboxItem,
) -> Result<()> {
    let room_id = parse_matrix_room_notify_ref(&item.notify_ref)?;
    let room_id = OwnedRoomId::try_from(room_id)
        .with_context(|| format!("invalid Matrix room id in notify_ref {}", item.notify_ref))?;
    let room = client
        .get_room(&room_id)
        .with_context(|| format!("Matrix room {room_id} is not loaded or joined"))?;
    let content = RoomMessageEventContent::notice_plain(&item.body);
    room.send(content)
        .await
        .with_context(|| format!("failed to send Matrix outbox notification {}", item.id))?;
    Ok(())
}

async fn verify_device_with_client<F>(
    client: &Client,
    target_user_id: OwnedUserId,
    target_device_id: OwnedDeviceId,
    timeout: Duration,
    confirm_sas: &mut F,
) -> Result<MatrixDeviceVerificationResult>
where
    F: FnMut(&MatrixSasPresentation) -> Result<bool>,
{
    client
        .encryption()
        .request_user_identity(&target_user_id)
        .await
        .with_context(|| format!("failed to query Matrix identity for {}", target_user_id))?;
    let devices = client
        .encryption()
        .get_user_devices(&target_user_id)
        .await
        .with_context(|| format!("failed to read Matrix devices for {}", target_user_id))?;
    let device = devices.get(&target_device_id).with_context(|| {
        format!(
            "Matrix device {} for user {} was not found in the crypto store",
            target_device_id, target_user_id
        )
    })?;

    if device.is_verified() {
        return Ok(MatrixDeviceVerificationResult {
            user_id: target_user_id.to_string(),
            device_id: target_device_id.to_string(),
            flow_id: None,
            status: MatrixDeviceVerificationStatus::AlreadyVerified,
        });
    }

    let verification = device
        .request_verification_with_methods(vec![VerificationMethod::SasV1])
        .await
        .context("failed to start Matrix device verification request")?;
    let flow_id = verification.flow_id().to_string();

    let sas = match tokio::time::timeout(timeout, wait_for_sas_verification(client, &verification))
        .await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = verification.cancel().await;
            bail!(
                "timed out waiting for Matrix device {} to accept SAS verification",
                target_device_id
            );
        }
    };

    match tokio::time::timeout(
        timeout,
        run_sas_verification(client, &flow_id, &sas, confirm_sas),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = sas.cancel().await;
            bail!(
                "timed out waiting for Matrix SAS verification to finish for device {}",
                target_device_id
            );
        }
    }

    Ok(MatrixDeviceVerificationResult {
        user_id: target_user_id.to_string(),
        device_id: target_device_id.to_string(),
        flow_id: Some(flow_id),
        status: MatrixDeviceVerificationStatus::Verified,
    })
}

async fn accept_incoming_device_verification<F>(
    client: &Client,
    target_user_id: OwnedUserId,
    target_device_id: OwnedDeviceId,
    timeout: Duration,
    confirm_sas: &mut F,
) -> Result<MatrixDeviceVerificationResult>
where
    F: FnMut(&MatrixSasPresentation) -> Result<bool>,
{
    let flow_id = wait_for_incoming_verification_request(
        client,
        target_user_id.clone(),
        target_device_id.clone(),
        timeout,
    )
    .await?;
    let verification = client
        .encryption()
        .get_verification_request(&target_user_id, &flow_id)
        .await
        .with_context(|| {
            format!(
                "Matrix verification request {flow_id} from device {} was not found after sync",
                target_device_id
            )
        })?;
    ensure_verification_request_device(&verification, &target_device_id)?;
    if matches!(
        verification.state(),
        VerificationRequestState::Requested { .. }
    ) {
        verification
            .accept_with_methods(vec![VerificationMethod::SasV1])
            .await
            .context("failed to accept Matrix device verification request")?;
    }

    let sas = match tokio::time::timeout(timeout, wait_for_sas_verification(client, &verification))
        .await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = verification.cancel().await;
            bail!(
                "timed out waiting for Matrix device {} to start SAS verification",
                target_device_id
            );
        }
    };

    match tokio::time::timeout(
        timeout,
        run_sas_verification(client, &flow_id, &sas, confirm_sas),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = sas.cancel().await;
            bail!(
                "timed out waiting for Matrix SAS verification to finish for device {}",
                target_device_id
            );
        }
    }

    Ok(MatrixDeviceVerificationResult {
        user_id: target_user_id.to_string(),
        device_id: target_device_id.to_string(),
        flow_id: Some(flow_id),
        status: MatrixDeviceVerificationStatus::Verified,
    })
}

async fn wait_for_incoming_verification_request(
    client: &Client,
    target_user_id: OwnedUserId,
    target_device_id: OwnedDeviceId,
    timeout: Duration,
) -> Result<String> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    client.add_event_handler(move |event: ToDeviceKeyVerificationRequestEvent| {
        let sender = sender.clone();
        let target_user_id = target_user_id.clone();
        let target_device_id = target_device_id.clone();
        async move {
            if event.sender == target_user_id && event.content.from_device == target_device_id {
                let _ = sender.send(event.content.transaction_id.to_string());
            }
        }
    });

    tokio::time::timeout(timeout, async {
        loop {
            let sync_once = client.sync_once(verification_sync_settings());
            tokio::pin!(sync_once);
            tokio::select! {
                flow_id = receiver.recv() => {
                    return flow_id.context("Matrix verification request channel closed");
                }
                sync_result = &mut sync_once => {
                    sync_result.context("Matrix sync failed while waiting for incoming verification request")?;
                }
            }
        }
    })
    .await
    .context("timed out waiting for incoming Matrix verification request")?
}

fn ensure_verification_request_device(
    verification: &VerificationRequest,
    target_device_id: &OwnedDeviceId,
) -> Result<()> {
    match verification.state() {
        VerificationRequestState::Requested {
            other_device_data, ..
        }
        | VerificationRequestState::Ready {
            other_device_data, ..
        } => {
            if other_device_data.device_id() != target_device_id {
                bail!(
                    "incoming Matrix verification request came from device {}, expected {}",
                    other_device_data.device_id(),
                    target_device_id
                );
            }
            Ok(())
        }
        VerificationRequestState::Transitioned { verification } => {
            if let Some(sas) = verification.sas()
                && sas.other_device().device_id() != target_device_id
            {
                bail!(
                    "incoming Matrix SAS verification came from device {}, expected {}",
                    sas.other_device().device_id(),
                    target_device_id
                );
            }
            Ok(())
        }
        VerificationRequestState::Cancelled(info) => {
            bail!(
                "Matrix verification request was cancelled: {}",
                info.reason()
            )
        }
        VerificationRequestState::Done => Ok(()),
        VerificationRequestState::Created { .. } => {
            bail!("incoming Matrix verification request is unexpectedly marked as created by us")
        }
    }
}

async fn wait_for_sas_verification(
    client: &Client,
    verification: &VerificationRequest,
) -> Result<SasVerification> {
    if let Some(sas) = sas_from_request_state(verification, verification.state()).await? {
        return Ok(sas);
    }

    let mut changes = verification.changes();
    loop {
        let sync_once = client.sync_once(verification_sync_settings());
        tokio::pin!(sync_once);
        tokio::select! {
            state = changes.next() => {
                let Some(state) = state else {
                    bail!("Matrix verification request ended before SAS verification started");
                };
                if let Some(sas) = sas_from_request_state(verification, state).await? {
                    return Ok(sas);
                }
            }
            sync_result = &mut sync_once => {
                sync_result.context("Matrix sync failed while waiting for SAS verification")?;
            }
        }
    }
}

async fn sas_from_request_state(
    verification: &VerificationRequest,
    state: VerificationRequestState,
) -> Result<Option<SasVerification>> {
    match state {
        VerificationRequestState::Ready { .. } => {
            if let Some(sas) = verification.start_sas().await? {
                Ok(Some(sas))
            } else {
                Ok(None)
            }
        }
        VerificationRequestState::Transitioned { verification } => {
            if let Some(sas) = verification.sas() {
                Ok(Some(sas))
            } else {
                bail!("Matrix verification transitioned to an unsupported non-SAS flow")
            }
        }
        VerificationRequestState::Done => {
            bail!("Matrix verification request finished before SAS verification started")
        }
        VerificationRequestState::Cancelled(info) => {
            bail!(
                "Matrix verification request was cancelled: {}",
                info.reason()
            )
        }
        VerificationRequestState::Created { .. } | VerificationRequestState::Requested { .. } => {
            Ok(None)
        }
    }
}

async fn run_sas_verification<F>(
    client: &Client,
    flow_id: &str,
    sas: &SasVerification,
    confirm_sas: &mut F,
) -> Result<()>
where
    F: FnMut(&MatrixSasPresentation) -> Result<bool>,
{
    let mut confirmed = false;
    if handle_sas_state(flow_id, sas, sas.state(), &mut confirmed, confirm_sas).await? {
        return Ok(());
    }

    let mut changes = sas.changes();
    loop {
        let sync_once = client.sync_once(verification_sync_settings());
        tokio::pin!(sync_once);
        tokio::select! {
            state = changes.next() => {
                let Some(state) = state else {
                    bail!("Matrix SAS verification ended before a terminal state");
                };
                if handle_sas_state(flow_id, sas, state, &mut confirmed, confirm_sas).await? {
                    return Ok(());
                }
            }
            sync_result = &mut sync_once => {
                sync_result.context("Matrix sync failed during SAS verification")?;
            }
        }
    }
}

async fn handle_sas_state<F>(
    flow_id: &str,
    sas: &SasVerification,
    state: SasState,
    confirmed: &mut bool,
    confirm_sas: &mut F,
) -> Result<bool>
where
    F: FnMut(&MatrixSasPresentation) -> Result<bool>,
{
    match state {
        SasState::KeysExchanged { emojis, decimals } => {
            if !*confirmed {
                let presentation = sas_presentation(flow_id, sas, emojis, decimals);
                if confirm_sas(&presentation)? {
                    sas.confirm().await?;
                    *confirmed = true;
                } else {
                    sas.mismatch().await?;
                    bail!("Matrix SAS verification cancelled because the SAS did not match");
                }
            }
            Ok(false)
        }
        SasState::Done { .. } => Ok(true),
        SasState::Cancelled(info) => {
            bail!("Matrix SAS verification was cancelled: {}", info.reason())
        }
        SasState::Created { .. }
        | SasState::Started { .. }
        | SasState::Accepted { .. }
        | SasState::Confirmed => Ok(false),
    }
}

fn sas_presentation(
    flow_id: &str,
    sas: &SasVerification,
    emojis: Option<EmojiShortAuthString>,
    decimals: (u16, u16, u16),
) -> MatrixSasPresentation {
    MatrixSasPresentation {
        flow_id: flow_id.to_string(),
        user_id: sas.other_user_id().to_string(),
        device_id: sas.other_device().device_id().to_string(),
        emojis: emojis
            .map(|sas| sas.emojis.into_iter().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .map(|emoji| MatrixSasEmoji {
                symbol: emoji.symbol.to_string(),
                description: emoji.description.to_string(),
            })
            .collect(),
        decimals: Some(decimals),
    }
}

fn verification_sync_settings() -> SyncSettings {
    SyncSettings::default().timeout(Duration::from_secs(1))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MatrixReplyContext {
    thread_root_event_id: Option<String>,
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
        thread_root_event_id: thread_root_event_id.clone(),
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
    content.relates_to = Some(message_relation(context)?);
    room.send(content)
        .await
        .context("Matrix room send failed")?;
    Ok(())
}

async fn send_matrix_notice(room: &Room, context: &MatrixReplyContext, body: &str) -> Result<()> {
    let mut content = RoomMessageEventContent::notice_plain(body);
    content.relates_to = Some(message_relation(context)?);
    room.send(content)
        .await
        .context("Matrix room send failed")?;
    Ok(())
}

fn message_relation(context: &MatrixReplyContext) -> Result<MatrixMessageRelation> {
    let reply_to = OwnedEventId::try_from(context.reply_event_id.as_str())
        .with_context(|| format!("invalid Matrix reply event id {}", context.reply_event_id))?;

    if let Some(thread_root_event_id) = context.thread_root_event_id.as_deref() {
        let root = OwnedEventId::try_from(thread_root_event_id).with_context(|| {
            format!(
                "invalid Matrix thread root event id {}",
                thread_root_event_id
            )
        })?;
        Ok(Relation::Thread(Thread::reply(root, reply_to)))
    } else {
        Ok(Relation::Reply(Reply::with_event_id(reply_to)))
    }
}

fn matrix_session_from_config(config: &MatrixConfig) -> Result<MatrixSession> {
    if let Some(path) = config
        .session_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let session = load_matrix_session(path)?;
        validate_session_user(&session, &config.user_id)?;
        return Ok(session);
    }

    matrix_session_from_access_token(config)
}

async fn build_matrix_client(config: &MatrixConfig) -> Result<Client> {
    matrix_client_builder(config)
        .build()
        .await
        .context("failed to build Matrix client")
}

async fn restore_matrix_client_from_config(config: &MatrixConfig) -> Result<Client> {
    let session = matrix_session_from_config(config)?;
    restore_matrix_client(config, session).await
}

async fn restore_matrix_client(config: &MatrixConfig, session: MatrixSession) -> Result<Client> {
    let client = build_matrix_client(config).await?;
    client
        .matrix_auth()
        .restore_session(session, RoomLoadSettings::default())
        .await
        .context("failed to restore Matrix access-token session")?;
    Ok(client)
}

async fn build_matrix_auth_client(config: &MatrixConfig) -> Result<Client> {
    Client::builder()
        .homeserver_url(config.homeserver_url.as_str())
        .build()
        .await
        .context("failed to build Matrix auth client")
}

fn matrix_client_builder(config: &MatrixConfig) -> ClientBuilder {
    let builder = Client::builder().homeserver_url(config.homeserver_url.as_str());
    if let Some(path) = matrix_store_path(config) {
        builder.sqlite_store(path, None)
    } else {
        builder
    }
}

fn matrix_store_path(config: &MatrixConfig) -> Option<PathBuf> {
    config
        .store_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn matrix_session_from_access_token(config: &MatrixConfig) -> Result<MatrixSession> {
    let access_token = config
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|access_token| !access_token.is_empty())
        .context("matrix.access_token is required when matrix.session_path is not set")?;
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
            access_token: access_token.to_string(),
            refresh_token: None,
        },
    })
}

fn matrix_session_path(config: &MatrixConfig) -> Result<PathBuf> {
    config
        .session_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .context("matrix.session_path is required for password login")
}

fn validate_password_login_paths(
    session_path: &Path,
    store_path: Option<&Path>,
    replace_session: bool,
) -> Result<()> {
    if session_path.exists() && !replace_session {
        bail!(
            "Matrix session already exists: {}. Password login creates a new Matrix device; choose a fresh matrix.session_path and matrix.store_path, or pass --replace-session after archiving the old store",
            session_path.display()
        );
    }

    if let Some(store_path) = store_path
        && path_has_entries(store_path)?
    {
        bail!(
            "matrix.store_path is not empty: {}. Password login creates a new Matrix device; choose a fresh store path or archive the old store before replacing the session",
            store_path.display()
        );
    }

    Ok(())
}

fn path_has_entries(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if path.is_file() {
        return Ok(true);
    }
    let mut entries = std::fs::read_dir(path)
        .with_context(|| format!("failed to read Matrix store dir {}", path.display()))?;
    Ok(entries.next().transpose()?.is_some())
}

fn load_matrix_session(path: impl AsRef<Path>) -> Result<MatrixSession> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read Matrix session {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("failed to parse Matrix session {}", path.display()))
}

fn save_matrix_session(path: impl AsRef<Path>, session: &MatrixSession) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create Matrix session dir {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(session).context("failed to serialize Matrix session")?;
    write_private_file(path, &bytes)
        .with_context(|| format!("failed to write Matrix session {}", path.display()))
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}

fn validate_session_user(session: &MatrixSession, expected_user_id: &str) -> Result<()> {
    if session.meta.user_id.as_str() != expected_user_id {
        anyhow::bail!(
            "Matrix session user {} does not match config user {}",
            session.meta.user_id,
            expected_user_id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
