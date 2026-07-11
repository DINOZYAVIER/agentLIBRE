use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, OperationKind, ProviderDeclaration, ProviderId, SensitiveInput, StateEffect,
};
use agl_content::{
    ArtifactRetention, ArtifactSensitivity, ArtifactSource, ArtifactSourceKind, Content,
    ContentPart, ImageDimensions, MediaType,
};
use agl_ids::RunId;
use agl_store::AglStore;
use image::{GenericImageView, ImageFormat, ImageReader, Limits};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

pub const PROVIDER_ID: &str = "screen-tools";
pub const SCREEN_CAPTURE_TOOL_ID: &str = "screen.capture";

const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

pub struct CapturedScreen {
    bytes: Vec<u8>,
    cleanup_path: Option<PathBuf>,
}

impl CapturedScreen {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            cleanup_path: None,
        }
    }

    fn portal_file(bytes: Vec<u8>, cleanup_path: Option<PathBuf>) -> Self {
        Self {
            bytes,
            cleanup_path,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub trait ScreenCaptureBackend: Send + Sync {
    fn probe(&self) -> Result<(), ScreenCaptureError>;
    fn capture(&self) -> Result<CapturedScreen, ScreenCaptureError>;

    fn cleanup(&self, _capture: &mut CapturedScreen) -> Result<(), ScreenCaptureError> {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct PortalScreenCaptureBackend;

impl ScreenCaptureBackend for PortalScreenCaptureBackend {
    fn probe(&self) -> Result<(), ScreenCaptureError> {
        portal_probe()
    }

    fn capture(&self) -> Result<CapturedScreen, ScreenCaptureError> {
        let uri = portal_screenshot_uri()?;
        let url = url::Url::parse(&uri).map_err(|_| ScreenCaptureError::InvalidPortalUri)?;
        if url.scheme() != "file" {
            return Err(ScreenCaptureError::InvalidPortalUri);
        }
        let path = url
            .to_file_path()
            .map_err(|_| ScreenCaptureError::InvalidPortalUri)?;
        let bytes = read_bounded_file(&path)?;
        let cleanup_path = owned_temporary_file(&path);
        Ok(CapturedScreen::portal_file(bytes, cleanup_path))
    }

    fn cleanup(&self, capture: &mut CapturedScreen) -> Result<(), ScreenCaptureError> {
        let Some(path) = capture.cleanup_path.take() else {
            return Ok(());
        };
        std::fs::remove_file(path).map_err(|error| ScreenCaptureError::Cleanup(error.to_string()))
    }
}

#[derive(Clone)]
pub struct ScreenTools {
    store_root: PathBuf,
    admitted_run: Option<RunId>,
    backend: Arc<dyn ScreenCaptureBackend>,
}

impl ScreenTools {
    pub fn new(store_root: impl AsRef<Path>, admitted_run: Option<RunId>) -> Self {
        Self::with_backend(
            store_root,
            admitted_run,
            Arc::new(PortalScreenCaptureBackend),
        )
    }

    pub fn with_backend(
        store_root: impl AsRef<Path>,
        admitted_run: Option<RunId>,
        backend: Arc<dyn ScreenCaptureBackend>,
    ) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
            admitted_run,
            backend,
        }
    }

    pub fn capture(
        &self,
        invocation: &ActionInvocation,
    ) -> Result<ActionResult, ScreenCaptureError> {
        if invocation.capability_id.as_str() != SCREEN_CAPTURE_TOOL_ID {
            return Err(ScreenCaptureError::UnknownCapability);
        }
        serde_json::from_value::<CaptureArgs>(invocation.arguments.clone())
            .map_err(|_| ScreenCaptureError::InvalidArguments)?;
        if self.admitted_run.as_ref() != Some(invocation.scope.run_id()) {
            return Err(ScreenCaptureError::PermissionDenied);
        }
        self.backend.probe()?;
        let mut capture = match self.backend.capture() {
            Ok(capture) => capture,
            Err(ScreenCaptureError::Cancelled) => {
                return Ok(ActionResult::new(json!({
                    "tool": SCREEN_CAPTURE_TOOL_ID,
                    "status": "cancelled",
                    "reason": "portal_cancelled",
                })));
            }
            Err(error) => return Err(error),
        };
        let normalized = normalize_image(capture.bytes())?;
        let store = AglStore::open_current_at(&self.store_root)
            .map_err(|error| ScreenCaptureError::Store(error.to_string()))?;
        let stored = store
            .write_artifact(
                invocation.scope.run_id(),
                MediaType::ImagePng,
                &normalized.png,
                Some(normalized.dimensions),
                ArtifactSensitivity::Sensitive,
                ArtifactSource {
                    kind: ArtifactSourceKind::ScreenCapture,
                    provider: Some("xdg-desktop-portal".to_string()),
                },
                ArtifactRetention::RunScoped,
            )
            .map_err(|error| ScreenCaptureError::Store(error.to_string()))?;
        let _ = self.backend.cleanup(&mut capture);
        let reference = stored.reference;
        let content = Content::new([ContentPart::artifact(reference.clone())])
            .map_err(|error| ScreenCaptureError::InvalidImage(error.to_string()))?;
        Ok(ActionResult::new(json!({
            "tool": SCREEN_CAPTURE_TOOL_ID,
            "status": "captured",
            "artifact_id": reference.artifact_id,
            "digest": reference.digest,
            "media_type": reference.media_type.mime(),
            "byte_length": reference.byte_length,
            "width": normalized.dimensions.width,
            "height": normalized.dimensions.height,
        }))
        .with_content(content))
    }
}

impl ActionHandler for ScreenTools {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError> {
        self.capture(&invocation)
            .map_err(|error| Box::new(error) as ActionHandlerError)
    }
}

pub fn provider_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| PortalScreenCaptureBackend.probe().is_ok())
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin screen provider id is valid"),
        "Screen Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin screen provider declaration is valid")
    .with_action(
        ActionDeclaration::from_schema::<CaptureArgs>(
            CapabilityId::new(SCREEN_CAPTURE_TOOL_ID)
                .expect("builtin screen capability id is valid"),
            "Capture one user-approved screen snapshot through the desktop portal.",
            OperationKind::Read,
        )
        .expect("builtin screen capture declaration is valid")
        .with_state_effects([StateEffect::HostScreenCapture])
        .with_sensitive_inputs([SensitiveInput::ScreenCapture]),
    )
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CaptureArgs {}

#[derive(Debug)]
struct NormalizedImage {
    png: Vec<u8>,
    dimensions: ImageDimensions,
}

fn normalize_image(bytes: &[u8]) -> Result<NormalizedImage, ScreenCaptureError> {
    if bytes.is_empty() {
        return Err(ScreenCaptureError::InvalidImage(
            "captured image is empty".to_string(),
        ));
    }
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(ScreenCaptureError::SourceTooLarge);
    }
    let (format, width, height) = image_header(bytes)?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err(ScreenCaptureError::UnsupportedFormat);
    }
    let dimensions = ImageDimensions::new(width, height)
        .map_err(|error| ScreenCaptureError::InvalidImage(error.to_string()))?;
    if dimensions.pixels() > MAX_IMAGE_PIXELS {
        return Err(ScreenCaptureError::ImageTooLarge);
    }

    let reader = image_reader(bytes)?;
    let decoded = reader
        .decode()
        .map_err(|error| ScreenCaptureError::InvalidImage(error.to_string()))?;
    if decoded.dimensions() != (width, height) {
        return Err(ScreenCaptureError::InvalidImage(
            "decoded dimensions changed".to_string(),
        ));
    }
    let sanitized = image::DynamicImage::ImageRgba8(decoded.to_rgba8());
    let mut png = Cursor::new(Vec::new());
    sanitized
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|error| ScreenCaptureError::InvalidImage(error.to_string()))?;
    let png = png.into_inner();
    if png.len() > MAX_ENCODED_BYTES {
        return Err(ScreenCaptureError::EncodedTooLarge);
    }
    Ok(NormalizedImage { png, dimensions })
}

fn image_header(bytes: &[u8]) -> Result<(ImageFormat, u32, u32), ScreenCaptureError> {
    let reader = image_reader(bytes)?;
    let format = reader
        .format()
        .ok_or(ScreenCaptureError::UnsupportedFormat)?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| ScreenCaptureError::InvalidImage(error.to_string()))?;
    Ok((format, width, height))
}

fn image_reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, ScreenCaptureError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| ScreenCaptureError::InvalidImage(error.to_string()))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    Ok(reader)
}

fn read_bounded_file(path: &Path) -> Result<Vec<u8>, ScreenCaptureError> {
    let file = File::open(path).map_err(|error| ScreenCaptureError::Io(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take((MAX_SOURCE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ScreenCaptureError::Io(error.to_string()))?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(ScreenCaptureError::SourceTooLarge);
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn owned_temporary_file(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let source_metadata = std::fs::symlink_metadata(path).ok()?;
    if !source_metadata.file_type().is_file() || source_metadata.uid() != unsafe { libc::geteuid() }
    {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    let temporary_root = std::env::temp_dir().canonicalize().ok()?;
    if !canonical.starts_with(temporary_root) {
        return None;
    }
    Some(canonical)
}

#[cfg(not(target_os = "linux"))]
fn owned_temporary_file(_path: &Path) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "linux")]
fn portal_probe() -> Result<(), ScreenCaptureError> {
    run_portal_task(Some(Duration::from_secs(2)), || async {
        ashpd::desktop::screenshot::ScreenshotProxy::new()
            .await
            .map(|_| ())
    })
}

#[cfg(not(target_os = "linux"))]
fn portal_probe() -> Result<(), ScreenCaptureError> {
    Err(ScreenCaptureError::Unavailable)
}

#[cfg(target_os = "linux")]
fn portal_screenshot_uri() -> Result<String, ScreenCaptureError> {
    run_portal_task(None, || async {
        let request = ashpd::desktop::screenshot::Screenshot::request()
            .interactive(true)
            .modal(true)
            .send()
            .await?;
        let response = request.response()?;
        Ok(response.uri().as_str().to_string())
    })
}

#[cfg(not(target_os = "linux"))]
fn portal_screenshot_uri() -> Result<String, ScreenCaptureError> {
    Err(ScreenCaptureError::Unavailable)
}

#[cfg(target_os = "linux")]
fn run_portal_task<T, F, Fut>(timeout: Option<Duration>, task: F) -> Result<T, ScreenCaptureError>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ashpd::Result<T>> + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("agl-screen-portal".to_string())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| ScreenCaptureError::Portal(error.to_string()))
                .and_then(|runtime| runtime.block_on(task()).map_err(map_portal_error));
            let _ = sender.send(result);
        })
        .map_err(|error| ScreenCaptureError::Portal(error.to_string()))?;
    match timeout {
        Some(timeout) => receiver
            .recv_timeout(timeout)
            .map_err(|_| ScreenCaptureError::Unavailable)?,
        None => receiver
            .recv()
            .map_err(|_| ScreenCaptureError::Portal("portal worker stopped".to_string()))?,
    }
}

#[cfg(target_os = "linux")]
fn map_portal_error(error: ashpd::Error) -> ScreenCaptureError {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
            ScreenCaptureError::Cancelled
        }
        ashpd::Error::PortalNotFound(_) => ScreenCaptureError::Unavailable,
        other => ScreenCaptureError::Portal(other.to_string()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScreenCaptureError {
    Unavailable,
    Cancelled,
    PermissionDenied,
    UnknownCapability,
    InvalidArguments,
    InvalidPortalUri,
    SourceTooLarge,
    UnsupportedFormat,
    ImageTooLarge,
    EncodedTooLarge,
    InvalidImage(String),
    Io(String),
    Store(String),
    Portal(String),
    Cleanup(String),
}

impl Display for ScreenCaptureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("screen capture portal is unavailable"),
            Self::Cancelled => formatter.write_str("screen capture was cancelled"),
            Self::PermissionDenied => {
                formatter.write_str("screen capture has no sensitive-input grant for this run")
            }
            Self::UnknownCapability => formatter.write_str("unknown screen capability"),
            Self::InvalidArguments => formatter.write_str("screen capture arguments are invalid"),
            Self::InvalidPortalUri => formatter.write_str("portal returned an invalid file URI"),
            Self::SourceTooLarge => formatter.write_str("captured image exceeds source byte limit"),
            Self::UnsupportedFormat => formatter.write_str("captured image format is unsupported"),
            Self::ImageTooLarge => formatter.write_str("captured image exceeds pixel limit"),
            Self::EncodedTooLarge => {
                formatter.write_str("sanitized image exceeds encoded byte limit")
            }
            Self::InvalidImage(message) => {
                write!(formatter, "captured image is invalid: {message}")
            }
            Self::Io(message) => write!(formatter, "captured image could not be read: {message}"),
            Self::Store(message) => {
                write!(formatter, "captured image could not be stored: {message}")
            }
            Self::Portal(message) => write!(formatter, "screen capture portal failed: {message}"),
            Self::Cleanup(message) => {
                write!(formatter, "portal temporary cleanup failed: {message}")
            }
        }
    }
}

impl Error for ScreenCaptureError {}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agl_capabilities::{DeclarationDigest, PolicyHash};
    use agl_ids::ExecutionScope;
    use agl_store::{DurableRunDraft, RunBudget, RunKind};

    use super::*;

    enum FakeOutcome {
        Capture(Vec<u8>),
        Cancelled,
    }

    struct FakeBackend {
        available: bool,
        outcomes: Mutex<VecDeque<FakeOutcome>>,
        probes: AtomicUsize,
        captures: AtomicUsize,
        cleanups: AtomicUsize,
    }

    impl FakeBackend {
        fn new(available: bool, outcomes: impl IntoIterator<Item = FakeOutcome>) -> Self {
            Self {
                available,
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                probes: AtomicUsize::new(0),
                captures: AtomicUsize::new(0),
                cleanups: AtomicUsize::new(0),
            }
        }
    }

    impl ScreenCaptureBackend for FakeBackend {
        fn probe(&self) -> Result<(), ScreenCaptureError> {
            self.probes.fetch_add(1, Ordering::Relaxed);
            self.available
                .then_some(())
                .ok_or(ScreenCaptureError::Unavailable)
        }

        fn capture(&self) -> Result<CapturedScreen, ScreenCaptureError> {
            self.captures.fetch_add(1, Ordering::Relaxed);
            match self.outcomes.lock().unwrap().pop_front().unwrap() {
                FakeOutcome::Capture(bytes) => Ok(CapturedScreen::from_bytes(bytes)),
                FakeOutcome::Cancelled => Err(ScreenCaptureError::Cancelled),
            }
        }

        fn cleanup(&self, _capture: &mut CapturedScreen) -> Result<(), ScreenCaptureError> {
            self.cleanups.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agl-screen-{name}-{}-{}",
            std::process::id(),
            RunId::generate()
        ))
    }

    fn admitted_run(root: &Path) -> RunId {
        let store = AglStore::open_at(root).unwrap();
        let run_id = RunId::generate();
        store
            .admit_run(&DurableRunDraft {
                run_id: run_id.clone(),
                session_id: None,
                turn_id: None,
                kind: RunKind::Cron,
                priority: 0,
                input: json!({}),
                checkpoint: None,
                effective_policy_hash: None,
                budget: RunBudget::default(),
                not_before_ms: None,
            })
            .unwrap();
        run_id
    }

    fn invocation(run_id: &RunId) -> ActionInvocation {
        let provider = declaration();
        let action = provider
            .action(&CapabilityId::new(SCREEN_CAPTURE_TOOL_ID).unwrap())
            .unwrap();
        ActionInvocation::new(
            ExecutionScope::builder(run_id.clone()).build().unwrap(),
            CapabilityId::new(SCREEN_CAPTURE_TOOL_ID).unwrap(),
            provider.id.clone(),
            DeclarationDigest::parse(action.digest().as_str()).unwrap(),
            PolicyHash::parse(&format!("sha256:{}", "0".repeat(64))).unwrap(),
            json!({}),
        )
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::new_rgb8(width, height);
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, ImageFormat::Png).unwrap();
        output.into_inner()
    }

    #[test]
    fn permission_denial_happens_before_portal_probe() {
        let root = temp_root("denied");
        let run_id = admitted_run(&root);
        let backend = Arc::new(FakeBackend::new(true, [FakeOutcome::Capture(png(2, 2))]));
        let tools = ScreenTools::with_backend(&root, None, backend.clone());

        let error = tools.capture(&invocation(&run_id)).unwrap_err();

        assert_eq!(error, ScreenCaptureError::PermissionDenied);
        assert_eq!(backend.probes.load(Ordering::Relaxed), 0);
        assert_eq!(backend.captures.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn portal_cancel_returns_typed_result_without_artifact() {
        let root = temp_root("cancelled");
        let run_id = admitted_run(&root);
        let backend = Arc::new(FakeBackend::new(true, [FakeOutcome::Cancelled]));
        let tools = ScreenTools::with_backend(&root, Some(run_id.clone()), backend.clone());

        let result = tools.capture(&invocation(&run_id)).unwrap();

        assert_eq!(result.data["status"], "cancelled");
        assert!(result.content.is_none());
        assert_eq!(backend.cleanups.load(Ordering::Relaxed), 0);
        assert!(!root.join("blobs").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn capture_sanitizes_stores_and_returns_one_sensitive_reference() {
        let root = temp_root("success");
        let run_id = admitted_run(&root);
        let backend = Arc::new(FakeBackend::new(true, [FakeOutcome::Capture(png(3, 2))]));
        let tools = ScreenTools::with_backend(&root, Some(run_id.clone()), backend.clone());

        let result = tools.capture(&invocation(&run_id)).unwrap();
        let content = result.content.unwrap();
        let reference = content.artifacts().next().unwrap();
        let store = AglStore::open_current_at(&root).unwrap();
        let resolved = store.resolve_artifact(&run_id, reference).unwrap();

        assert_eq!(content.artifact_count(), 1);
        assert_eq!(reference.media_type, MediaType::ImagePng);
        assert_eq!(reference.sensitivity, ArtifactSensitivity::Sensitive);
        assert_eq!(reference.image.unwrap().width, 3);
        assert_eq!(
            image::load_from_memory(&resolved.bytes)
                .unwrap()
                .dimensions(),
            (3, 2)
        );
        assert_eq!(backend.cleanups.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_and_oversized_sources_are_rejected_before_storage() {
        assert!(matches!(
            normalize_image(b"not an image"),
            Err(ScreenCaptureError::UnsupportedFormat | ScreenCaptureError::InvalidImage(_))
        ));
        assert_eq!(
            normalize_image(&vec![0; MAX_SOURCE_BYTES + 1]).unwrap_err(),
            ScreenCaptureError::SourceTooLarge
        );
    }
}
