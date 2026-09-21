use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use hf_hub::progress::{DownloadEvent, ProgressEvent, ProgressHandler};
use serde::{Deserialize, Deserializer, Serialize};
#[cfg(test)]
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt;
use url::Url;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModelArtifactDigest([u8; 32]);

impl ModelArtifactDigest {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let hex = value
            .strip_prefix("sha256:")
            .context("model digest requires sha256 prefix")?;
        ensure!(
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "model digest requires 64 lowercase hexadecimal characters"
        );
        let mut bytes = [0_u8; 32];
        for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = u8::from_str_radix(
                std::str::from_utf8(pair).expect("validated digest contains only ASCII"),
                16,
            )?;
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[cfg(test)]
    fn from_hasher(hasher: Sha256) -> Self {
        Self(hasher.finalize().into())
    }
}

impl std::fmt::Display for ModelArtifactDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("sha256:")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for ModelArtifactDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ModelArtifactDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelImportRequest {
    pub path: PathBuf,
    pub expected_digest: ModelArtifactDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelFetchRequest {
    pub url: Url,
    pub destination: PathBuf,
    pub expected_digest: ModelArtifactDigest,
    pub expected_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FetchProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Clone)]
pub struct ImportedModel {
    path: PathBuf,
    digest: ModelArtifactDigest,
    bytes: u64,
    file: Arc<std::fs::File>,
    identity: ModelFileIdentity,
    managed: bool,
}

impl ImportedModel {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn digest(&self) -> ModelArtifactDigest {
        self.digest
    }

    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn try_clone_file(&self) -> std::io::Result<std::fs::File> {
        self.verify_unchanged()?;
        self.file.try_clone()
    }

    pub fn verify_unchanged(&self) -> std::io::Result<()> {
        let file_metadata = self.file.metadata()?;
        let path_metadata = std::fs::symlink_metadata(&self.path)?;
        if path_metadata.file_type().is_symlink()
            || !path_metadata.is_file()
            || (self.managed && !managed_model_metadata_is_safe(&file_metadata))
            || (self.managed && !managed_model_metadata_is_safe(&path_metadata))
            || model_file_identity(&file_metadata)? != self.identity
            || model_file_identity(&path_metadata)? != self.identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "verified model file changed",
            ));
        }
        Ok(())
    }
}

pub(crate) fn open_managed_model(
    path: &Path,
    expected_digest: ModelArtifactDigest,
    expected_bytes: u64,
) -> Result<ImportedModel> {
    validate_model_path(path)?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure_managed_model_metadata(&metadata)?;
    ensure!(metadata.len() == expected_bytes, "model size mismatch");

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;

        let mut options = std::fs::OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::File::open(path)?;

    let opened_metadata = file.metadata()?;
    ensure_managed_model_metadata(&opened_metadata)?;
    ensure!(
        same_file_identity(&metadata, &opened_metadata),
        "model path changed while it was being opened"
    );
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .context("model is shorter than the GGUF header")?;
    ensure!(&magic == b"GGUF", "model does not contain GGUF magic");
    let canonical = path.canonicalize()?;
    let path_metadata = std::fs::symlink_metadata(&canonical)?;
    ensure_managed_model_metadata(&path_metadata)?;
    ensure!(
        same_file_identity(&opened_metadata, &path_metadata),
        "model path changed while it was being opened"
    );
    let identity = model_file_identity(&opened_metadata)?;
    Ok(ImportedModel {
        path: canonical,
        digest: expected_digest,
        bytes: expected_bytes,
        file: Arc::new(file),
        identity,
        managed: true,
    })
}

impl std::fmt::Debug for ImportedModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImportedModel")
            .field("path", &self.path)
            .field("digest", &self.digest)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ImportedModel {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.digest == other.digest
            && self.bytes == other.bytes
            && self.identity == other.identity
            && self.managed == other.managed
    }
}

impl Eq for ImportedModel {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModelFileIdentity {
    bytes: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(not(unix))]
    modified_nanoseconds: u128,
}

pub fn import_model(request: &ModelImportRequest) -> Result<ImportedModel> {
    validate_model_path(&request.path)?;
    let metadata = std::fs::symlink_metadata(&request.path)
        .with_context(|| format!("failed to inspect {}", request.path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "model path cannot be a symlink"
    );
    ensure!(metadata.is_file(), "model path must be a regular file");
    let mut file = std::fs::File::open(&request.path)?;
    let opened_metadata = file.metadata()?;
    ensure!(
        same_file_identity(&metadata, &opened_metadata),
        "model path changed before verification"
    );
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .context("model is shorter than the GGUF header")?;
    ensure!(&magic == b"GGUF", "model does not contain GGUF magic");
    let verified_metadata = file.metadata()?;
    ensure!(
        model_file_identity(&opened_metadata)? == model_file_identity(&verified_metadata)?,
        "model changed while it was being verified"
    );
    let canonical = request.path.canonicalize()?;
    let path_metadata = std::fs::symlink_metadata(&canonical)?;
    ensure!(
        path_metadata.is_file()
            && !path_metadata.file_type().is_symlink()
            && same_file_identity(&verified_metadata, &path_metadata),
        "model path changed while it was being verified"
    );
    let identity = model_file_identity(&verified_metadata)?;
    Ok(ImportedModel {
        path: canonical,
        digest: request.expected_digest,
        bytes: verified_metadata.len(),
        file: Arc::new(file),
        identity,
        managed: false,
    })
}

pub async fn fetch_model(
    request: &ModelFetchRequest,
    cancelled: &AtomicBool,
    progress: impl FnMut(FetchProgress) + Send + 'static,
) -> Result<ImportedModel> {
    validate_fetch_request(request)?;
    ensure!(!cancelled.load(Ordering::Acquire), "model fetch cancelled");
    if request.destination.exists() {
        return open_managed_model(
            &request.destination,
            request.expected_digest,
            request.expected_bytes,
        );
    }
    let parent = request
        .destination
        .parent()
        .context("model destination requires a parent directory")?;
    tokio::fs::create_dir_all(parent).await?;
    let source = parse_hugging_face_url(&request.url)?;
    let cache = parent.join("hf-hub-cache");
    let progress = Arc::new(std::sync::Mutex::new(progress));
    let client = hf_hub::HFClient::builder()
        .endpoint("https://huggingface.co")
        .cache_dir(cache)
        .build()?;
    let repository = client.model(source.owner, source.repository);
    let filename = source.filename;
    let revision = source.revision;
    let metadata = repository
        .get_file_metadata()
        .filepath(filename.clone())
        .revision(&revision)
        .send()
        .await?;
    ensure!(!cancelled.load(Ordering::Acquire), "model fetch cancelled");
    ensure!(
        metadata.file_size == request.expected_bytes,
        "Hub model size differs from the pinned artifact size"
    );
    let download = repository
        .download_file()
        .filename(filename)
        .revision(revision)
        .progress(HubProgress {
            callback: progress.clone(),
        })
        .send();
    tokio::pin!(download);
    let cached = loop {
        tokio::select! {
            result = &mut download => break result?,
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                ensure!(!cancelled.load(Ordering::Acquire), "model fetch cancelled");
            }
        }
    };
    ensure!(!cancelled.load(Ordering::Acquire), "model fetch cancelled");
    let partial = request.destination.with_extension("agl-install");
    match std::fs::symlink_metadata(&partial) {
        Ok(metadata) => {
            ensure_managed_model_metadata(&metadata)?;
            tokio::fs::remove_file(&partial).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&partial)
        .await
        .context("another fetch already owns the partial model path")?;
    let outcome = async {
        let mut source = tokio::fs::File::open(&cached).await?;
        let mut downloaded_bytes = 0_u64;
        let mut magic = Vec::with_capacity(4);
        let mut chunk = vec![0_u8; 1024 * 1024];
        loop {
            ensure!(!cancelled.load(Ordering::Acquire), "model fetch cancelled");
            let read = tokio::io::AsyncReadExt::read(&mut source, &mut chunk).await?;
            if read == 0 {
                break;
            }
            let chunk = &chunk[..read];
            downloaded_bytes = downloaded_bytes
                .checked_add(chunk.len() as u64)
                .context("model size overflow")?;
            ensure!(
                downloaded_bytes <= request.expected_bytes,
                "model fetch exceeds expected size"
            );
            if magic.len() < 4 {
                magic.extend_from_slice(&chunk[..chunk.len().min(4 - magic.len())]);
            }
            file.write_all(chunk).await?;
            if let Ok(mut callback) = progress.lock() {
                callback(FetchProgress {
                    downloaded_bytes,
                    total_bytes: Some(request.expected_bytes),
                });
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        ensure!(magic == b"GGUF", "model does not contain GGUF magic");
        ensure!(
            downloaded_bytes == request.expected_bytes,
            "model fetch size mismatch"
        );
        let file_metadata = file.metadata().await?;
        let verified_file = std::fs::File::open(&partial)?;
        ensure!(
            same_file_identity(&file_metadata, &verified_file.metadata()?),
            "partial model changed before installation"
        );
        drop(file);
        tokio::fs::rename(&partial, &request.destination).await?;
        tokio::fs::File::open(parent).await?.sync_all().await?;
        let installed_metadata = verified_file.metadata()?;
        let imported = ImportedModel {
            path: request.destination.canonicalize()?,
            digest: request.expected_digest,
            bytes: downloaded_bytes,
            file: Arc::new(verified_file),
            identity: model_file_identity(&installed_metadata)?,
            managed: true,
        };
        imported.verify_unchanged()?;
        Ok(imported)
    }
    .await;
    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
    }
    outcome
}

struct HuggingFaceSource {
    owner: String,
    repository: String,
    revision: String,
    filename: String,
}

fn parse_hugging_face_url(url: &Url) -> Result<HuggingFaceSource> {
    ensure!(
        url.scheme() == "https" && url.host_str() == Some("huggingface.co") && url.port().is_none(),
        "model fetch requires a canonical huggingface.co URL"
    );
    let segments = url
        .path_segments()
        .context("model fetch URL has no path")?
        .collect::<Vec<_>>();
    ensure!(
        segments.len() >= 5 && segments[2] == "resolve",
        "model fetch URL must identify one repository file revision"
    );
    ensure!(
        segments.iter().all(|segment| {
            !segment.is_empty() && *segment != "." && *segment != ".." && !segment.contains('%')
        }),
        "model fetch URL contains an invalid path segment"
    );
    Ok(HuggingFaceSource {
        owner: segments[0].to_owned(),
        repository: segments[1].to_owned(),
        revision: segments[3].to_owned(),
        filename: segments[4..].join("/"),
    })
}

struct HubProgress<F> {
    callback: Arc<std::sync::Mutex<F>>,
}

impl<F> ProgressHandler for HubProgress<F>
where
    F: FnMut(FetchProgress) + Send,
{
    fn on_progress(&self, event: &ProgressEvent) {
        let (downloaded_bytes, total_bytes) = match event {
            ProgressEvent::Download(DownloadEvent::Start { total_bytes, .. }) => {
                (0, Some(*total_bytes))
            }
            ProgressEvent::Download(DownloadEvent::AggregateProgress {
                bytes_completed,
                total_bytes,
                ..
            }) => (*bytes_completed, Some(*total_bytes)),
            ProgressEvent::Download(DownloadEvent::Progress { files }) => {
                let downloaded = files.iter().map(|file| file.bytes_completed).sum();
                let total = files.iter().map(|file| file.total_bytes).sum();
                (downloaded, (total > 0).then_some(total))
            }
            _ => return,
        };
        if let Ok(mut callback) = self.callback.lock() {
            callback(FetchProgress {
                downloaded_bytes,
                total_bytes,
            });
        }
    }
}

fn validate_fetch_request(request: &ModelFetchRequest) -> Result<()> {
    ensure!(
        request.url.scheme() == "https",
        "model fetch requires HTTPS"
    );
    ensure!(
        request.expected_bytes > 0 && request.expected_bytes <= 1024_u64.pow(4),
        "expected model size must be between 1 byte and 1 TiB"
    );
    ensure!(
        request.url.username().is_empty() && request.url.password().is_none(),
        "model fetch URL cannot contain credentials"
    );
    ensure!(
        request.url.query().is_none() && request.url.fragment().is_none(),
        "model fetch URL cannot contain query parameters or fragments"
    );
    validate_model_path(&request.destination)
}

fn validate_model_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "model path must be absolute");
    ensure!(
        path.extension().and_then(|value| value.to_str()) == Some("gguf"),
        "model path must use the .gguf extension"
    );
    Ok(())
}

fn ensure_managed_model_metadata(metadata: &std::fs::Metadata) -> Result<()> {
    ensure!(metadata.is_file(), "model path must be a regular file");
    ensure!(
        managed_model_metadata_is_safe(metadata),
        "managed model must be owned by the current user, have one link, and deny group/other access"
    );
    Ok(())
}

#[cfg(unix)]
fn managed_model_metadata_is_safe(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    metadata.uid() == unsafe { libc::geteuid() }
        && metadata.nlink() == 1
        && metadata.mode() & 0o077 == 0
}

#[cfg(not(unix))]
fn managed_model_metadata_is_safe(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    model_file_identity(left).ok() == model_file_identity(right).ok()
}

#[cfg(not(unix))]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    model_file_identity(left).ok() == model_file_identity(right).ok()
}

#[cfg(unix)]
fn model_file_identity(metadata: &std::fs::Metadata) -> std::io::Result<ModelFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(ModelFileIdentity {
        bytes: metadata.len(),
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(not(unix))]
fn model_file_identity(metadata: &std::fs::Metadata) -> std::io::Result<ModelFileIdentity> {
    let modified_nanoseconds = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "model modification time predates Unix epoch",
            )
        })?
        .as_nanos();
    Ok(ModelFileIdentity {
        bytes: metadata.len(),
        modified_nanoseconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_is_explicit_and_metadata_checked() {
        let root = std::env::temp_dir().join(format!(
            "agl-runtime-model-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("model.gguf");
        std::fs::write(&path, b"GGUF-test").unwrap();
        let digest = ModelArtifactDigest::from_hasher({
            let mut hasher = Sha256::new();
            hasher.update(b"GGUF-test");
            hasher
        });
        let imported = import_model(&ModelImportRequest {
            path,
            expected_digest: digest,
        })
        .unwrap();
        assert_eq!(imported.digest(), digest);
        assert_eq!(imported.bytes(), 9);
        let invalid = root.join("invalid.gguf");
        std::fs::write(&invalid, b"nope").unwrap();
        let digest = ModelArtifactDigest::from_hasher({
            let mut hasher = Sha256::new();
            hasher.update(b"nope");
            hasher
        });
        let invalid_import = import_model(&ModelImportRequest {
            path: invalid,
            expected_digest: digest,
        });
        assert!(invalid_import.is_err());
        std::fs::write(imported.path(), b"GGUF-changed").unwrap();
        assert!(imported.verify_unchanged().is_err());
        assert!(imported.try_clone_file().is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn managed_cache_open_checks_metadata() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "agl-runtime-managed-model-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("model.gguf");
        std::fs::write(&path, b"GGUF-cache").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let declared_digest = ModelArtifactDigest::from_hasher({
            let mut hasher = Sha256::new();
            hasher.update(b"GGUF-cache");
            hasher
        });

        let cached = open_managed_model(&path, declared_digest, 10).unwrap();
        assert_eq!(cached.digest(), declared_digest);
        assert_eq!(cached.bytes(), 10);
        let wrong_digest =
            ModelArtifactDigest::parse(format!("sha256:{}", "7".repeat(64))).unwrap();
        assert_eq!(
            open_managed_model(&path, wrong_digest, 10)
                .unwrap()
                .digest(),
            wrong_digest
        );

        let alias = root.join("alias.gguf");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(cached.verify_unchanged().is_err());
        assert!(open_managed_model(&path, declared_digest, 10).is_err());
        std::fs::remove_file(alias).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_managed_model(&path, declared_digest, 10).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hugging_face_source_url_is_exact_and_selects_one_revision_file() {
        let source = parse_hugging_face_url(
            &Url::parse(
                "https://huggingface.co/owner/model/resolve/0123456789abcdef/weights/model.gguf",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(source.owner, "owner");
        assert_eq!(source.repository, "model");
        assert_eq!(source.revision, "0123456789abcdef");
        assert_eq!(source.filename, "weights/model.gguf");

        for invalid in [
            "https://example.com/owner/model/resolve/rev/model.gguf",
            "https://huggingface.co/owner/model/blob/rev/model.gguf",
            "https://huggingface.co/owner/model/resolve/rev",
            "https://huggingface.co/owner/model/resolve/rev/a%20b.gguf",
        ] {
            assert!(parse_hugging_face_url(&Url::parse(invalid).unwrap()).is_err());
        }
    }
}
