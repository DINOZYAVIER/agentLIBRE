//! Host-side discovery for the exact Linux resources admitted to an inference worker.
//!
//! This module deliberately does not load Vulkan, llama.cpp, or ggml. GPU render
//! nodes are identified through kernel device numbers and sysfs evidence. Vulkan
//! driver manifests are accepted only through an explicit loader override; there
//! is no default-directory scan and no shared-object search-path fallback.

#![cfg(target_os = "linux")]

mod elf;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

pub const VK_DRIVER_FILES: &str = "VK_DRIVER_FILES";
pub const VK_ICD_FILENAMES: &str = "VK_ICD_FILENAMES";

const DRM_RENDER_MAJOR: u64 = 226;
const MAX_DRIVER_MANIFESTS: usize = 16;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 32 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_SYSFS_ATTRIBUTE_BYTES: usize = 64 * 1024;
const MAX_UEVENT_FIELDS: usize = 128;
const NATIVE_BUNDLE_BASE_DIRECTORY: &str = "agl-inference-native";
const NATIVE_BUNDLE_DIRECTORY_PREFIX: &str = "sha256-";
const NATIVE_BUNDLE_DIGEST_HEX_BYTES: usize = 64;
const MAX_NATIVE_BUNDLE_FILES: usize = 64;
const REQUIRED_NATIVE_LIBRARIES: [&str; 5] = [
    "libllama-common.so.0",
    "libmtmd.so.0",
    "libllama.so.0",
    "libggml.so.0",
    "libggml-base.so.0",
];
const NATIVE_BUNDLE_ID_DOMAIN: &[u8] = b"agl-inference-native-bundle-v1\0";
const MAX_NATIVE_BUNDLE_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_NATIVE_BUNDLE_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerResourceErrorCode {
    InvalidEnvironment,
    InvalidPath,
    InvalidManifest,
    InvalidRenderNode,
    MissingSysfsEvidence,
    Io,
}

impl WorkerResourceErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidEnvironment => "invalid_environment",
            Self::InvalidPath => "invalid_path",
            Self::InvalidManifest => "invalid_manifest",
            Self::InvalidRenderNode => "invalid_render_node",
            Self::MissingSysfsEvidence => "missing_sysfs_evidence",
            Self::Io => "io",
        }
    }
}

#[derive(Debug)]
pub struct WorkerResourceError {
    code: WorkerResourceErrorCode,
    message: String,
}

impl WorkerResourceError {
    fn new(code: WorkerResourceErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub const fn code(&self) -> WorkerResourceErrorCode {
        self.code
    }
}

impl fmt::Display for WorkerResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for WorkerResourceError {}

pub type Result<T> = std::result::Result<T, WorkerResourceError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverBuildEvidenceSource {
    KernelModuleBuildId,
    KernelModuleAttributes,
    KernelBuildId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderDeviceResource {
    render_node: PathBuf,
    sysfs_device_root: PathBuf,
    physical_device_id: String,
    pci_device_id: Option<String>,
    pci_subsystem_id: Option<String>,
    driver_build_id: String,
    driver_build_evidence_source: DriverBuildEvidenceSource,
    vram_total_path: Option<PathBuf>,
    vram_used_path: Option<PathBuf>,
    device_major: u64,
    device_minor: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceMemoryObservation {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

impl RenderDeviceResource {
    #[cfg(test)]
    pub(crate) fn fixture(
        render_node: impl Into<PathBuf>,
        physical_device_id: impl Into<String>,
        driver_build_id: impl Into<String>,
    ) -> Self {
        Self {
            render_node: render_node.into(),
            sysfs_device_root: PathBuf::from("/sys/devices/fixture"),
            physical_device_id: physical_device_id.into(),
            pci_device_id: None,
            pci_subsystem_id: None,
            driver_build_id: driver_build_id.into(),
            driver_build_evidence_source: DriverBuildEvidenceSource::KernelModuleBuildId,
            vram_total_path: None,
            vram_used_path: None,
            device_major: DRM_RENDER_MAJOR,
            device_minor: 128,
        }
    }

    pub fn render_node(&self) -> &Path {
        &self.render_node
    }

    /// Canonical kernel-owned sysfs directory for this exact DRM device.
    ///
    /// Vulkan drivers use this metadata after opening the admitted render node.
    /// Keeping the canonical per-device subtree here avoids granting the worker
    /// broad read access to `/sys`.
    pub fn sysfs_device_root(&self) -> &Path {
        &self.sysfs_device_root
    }

    pub fn physical_device_id(&self) -> &str {
        &self.physical_device_id
    }

    pub fn pci_device_id(&self) -> Option<&str> {
        self.pci_device_id.as_deref()
    }

    pub fn pci_subsystem_id(&self) -> Option<&str> {
        self.pci_subsystem_id.as_deref()
    }

    pub fn driver_build_id(&self) -> &str {
        &self.driver_build_id
    }

    pub const fn driver_build_evidence_source(&self) -> DriverBuildEvidenceSource {
        self.driver_build_evidence_source
    }

    /// Reads a fresh kernel-owned VRAM observation when the DRM driver exposes
    /// both total and used byte counters. Missing counters are an explicit
    /// unknown result; malformed or internally impossible counters fail.
    pub fn memory_observation(&self) -> Result<Option<DeviceMemoryObservation>> {
        let (Some(total_path), Some(used_path)) = (&self.vram_total_path, &self.vram_used_path)
        else {
            return Ok(None);
        };
        let total_bytes = read_canonical_u64_sysfs(total_path, "VRAM total")?;
        let used_bytes = read_canonical_u64_sysfs(used_path, "VRAM used")?;
        if total_bytes == 0 || used_bytes > total_bytes {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "DRM VRAM counters report an impossible memory snapshot",
            ));
        }
        Ok(Some(DeviceMemoryObservation {
            total_bytes,
            used_bytes,
            available_bytes: total_bytes - used_bytes,
        }))
    }

    pub const fn device_major(&self) -> u64 {
        self.device_major
    }

    pub const fn device_minor(&self) -> u64 {
        self.device_minor
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanDriverManifestResource {
    manifest_path: PathBuf,
    library_path: PathBuf,
}

impl VulkanDriverManifestResource {
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn library_path(&self) -> &Path {
        &self.library_path
    }
}

/// A closed environment overlay for the inference worker.
///
/// Values can contain only the effective Vulkan driver-manifest override. The
/// type has no public constructor or mutation method so callers cannot turn it
/// into a general daemon-environment forwarding channel.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkerLaunchEnvironment {
    values: BTreeMap<String, String>,
}

impl WorkerLaunchEnvironment {
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn to_process_environment(&self) -> BTreeMap<String, OsString> {
        self.values
            .iter()
            .map(|(name, value)| (name.clone(), OsString::from(value)))
            .collect()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkerLaunchResources {
    render_devices: Vec<RenderDeviceResource>,
    driver_manifests: Vec<VulkanDriverManifestResource>,
    runtime_roots: Vec<PathBuf>,
    environment: WorkerLaunchEnvironment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeBundleLaunchResources {
    directory: PathBuf,
    external_dependencies: Vec<PathBuf>,
    identity: String,
}

impl NativeBundleLaunchResources {
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn external_dependencies(&self) -> &[PathBuf] {
        &self.external_dependencies
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }
}

pub(crate) fn discover_native_bundle_for_worker(
    worker_elf: &Path,
    bundle_anchor: &Path,
    include_accelerator_dependencies: bool,
) -> Result<NativeBundleLaunchResources> {
    discover_native_bundle_for_worker_inner(
        worker_elf,
        None,
        bundle_anchor,
        include_accelerator_dependencies,
    )
}

/// Inspects an already-open worker inode through its proc-fd path without
/// canonicalizing that magic link. The separate origin is captured when the
/// inode is admitted, so `$ORIGIN` remains exact even if an atomic deployment
/// replaces or unlinks the directory entry before spawn.
pub(crate) fn discover_native_bundle_for_pinned_worker(
    worker_elf: &Path,
    worker_origin: &Path,
    bundle_anchor: &Path,
    include_accelerator_dependencies: bool,
) -> Result<NativeBundleLaunchResources> {
    discover_native_bundle_for_worker_inner(
        worker_elf,
        Some(worker_origin),
        bundle_anchor,
        include_accelerator_dependencies,
    )
}

fn discover_native_bundle_for_worker_inner(
    worker_elf: &Path,
    worker_origin: Option<&Path>,
    bundle_anchor: &Path,
    include_accelerator_dependencies: bool,
) -> Result<NativeBundleLaunchResources> {
    let parent = bundle_anchor.parent().ok_or_else(|| {
        WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            "inference worker executable has no sibling directory",
        )
    })?;
    let bundle_base = parent.join(NATIVE_BUNDLE_BASE_DIRECTORY);
    let directory = content_addressed_native_bundle_directory(
        worker_elf,
        worker_origin.unwrap_or(parent),
        &bundle_base,
    )?;
    let metadata = fs::symlink_metadata(&directory).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::InvalidPath,
            "failed to inspect native inference bundle directory",
            error,
        )
    })?;
    validate_native_bundle_metadata(&directory, &metadata, true)?;

    let mut names = BTreeSet::new();
    let mut plugins = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::InvalidPath,
            "failed to list native inference bundle directory",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            io_error(
                WorkerResourceErrorCode::InvalidPath,
                "failed to read native inference bundle entry",
                error,
            )
        })?;
        if names.len() >= MAX_NATIVE_BUNDLE_FILES {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                "native inference bundle exceeds its file-count bound",
            ));
        }
        let name = entry.file_name().into_string().map_err(|_| {
            WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                "native inference bundle contains a non-UTF-8 filename",
            )
        })?;
        let allowed = REQUIRED_NATIVE_LIBRARIES.contains(&name.as_str())
            || (name.starts_with("libggml-cpu-") && name.ends_with(".so"))
            || name == "libggml-vulkan.so";
        if !allowed || !names.insert(name.clone()) {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                format!("native inference bundle contains an unexpected file: {name}"),
            ));
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            io_error(
                WorkerResourceErrorCode::InvalidPath,
                "failed to inspect native inference bundle file",
                error,
            )
        })?;
        validate_native_bundle_metadata(&path, &metadata, false)?;
        if name.starts_with("libggml-cpu-")
            || (include_accelerator_dependencies && name == "libggml-vulkan.so")
        {
            plugins.push(path);
        }
    }
    if REQUIRED_NATIVE_LIBRARIES
        .iter()
        .any(|required| !names.contains(*required))
        || !names.iter().any(|name| name.starts_with("libggml-cpu-"))
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            "native inference bundle is incomplete",
        ));
    }
    plugins.sort();
    let identity = native_bundle_identity(&directory, &names)?;
    let expected_leaf = identity
        .strip_prefix("sha256:")
        .map(|digest| format!("{NATIVE_BUNDLE_DIRECTORY_PREFIX}{digest}"))
        .expect("native bundle identity always uses sha256");
    if directory.file_name() != Some(OsStr::new(&expected_leaf)) {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            "native inference bundle content address does not match its exact manifest",
        ));
    }
    let external_dependencies = match worker_origin {
        Some(origin) => {
            elf::dependency_closure_with_pinned_seed(worker_elf, origin, &plugins, Some(&directory))
        }
        None => {
            let mut closure_seeds = vec![worker_elf.to_path_buf()];
            closure_seeds.extend(plugins);
            elf::dependency_closure(&closure_seeds, Some(&directory))
        }
    }
    .map_err(|message| WorkerResourceError::new(WorkerResourceErrorCode::InvalidPath, message))?;
    if external_dependencies.iter().any(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("libggml"))
    }) {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            "native backend plugin resolves a ggml dependency outside its immutable bundle",
        ));
    }
    Ok(NativeBundleLaunchResources {
        directory,
        external_dependencies,
        identity,
    })
}

fn content_addressed_native_bundle_directory(
    worker_elf: &Path,
    worker_origin: &Path,
    bundle_base: &Path,
) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(bundle_base).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::InvalidPath,
            "failed to inspect native inference bundle base directory",
            error,
        )
    })?;
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.permissions().mode() & 0o022 != 0
        || (metadata.uid() != effective_uid && metadata.uid() != 0)
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            "native inference bundle base is not an exact owner-controlled directory",
        ));
    }

    let embedded = elf::native_bundle_search_directory(worker_elf, worker_origin, bundle_base)
        .map_err(|message| {
            WorkerResourceError::new(WorkerResourceErrorCode::InvalidPath, message)
        })?;
    let Some(name) = embedded.file_name().and_then(OsStr::to_str) else {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            "worker ELF native bundle leaf is not bounded UTF-8",
        ));
    };
    if !content_addressed_bundle_name(name) {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            "worker ELF native bundle leaf has a malformed content address",
        ));
    }
    Ok(embedded)
}

fn content_addressed_bundle_name(name: &str) -> bool {
    name.strip_prefix(NATIVE_BUNDLE_DIRECTORY_PREFIX)
        .is_some_and(|digest| {
            digest.len() == NATIVE_BUNDLE_DIGEST_HEX_BYTES
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn native_bundle_identity(directory: &Path, names: &BTreeSet<String>) -> Result<String> {
    let mut identity = Sha256::new();
    identity.update(NATIVE_BUNDLE_ID_DOMAIN);
    let mut total_bytes = 0_u64;
    for name in names {
        let path = directory.join(name);
        let length = fs::metadata(&path)
            .map_err(|error| {
                io_error(
                    WorkerResourceErrorCode::InvalidPath,
                    "failed to inspect native bundle identity input",
                    error,
                )
            })?
            .len();
        if length > MAX_NATIVE_BUNDLE_FILE_BYTES {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                "native bundle file exceeds its identity byte bound",
            ));
        }
        total_bytes = total_bytes.checked_add(length).ok_or_else(|| {
            WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                "native bundle identity byte count overflowed",
            )
        })?;
        if total_bytes > MAX_NATIVE_BUNDLE_TOTAL_BYTES {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                "native bundle exceeds its total identity byte bound",
            ));
        }
        let digest = sha256_regular_file(&path)?;
        hash_identity_frame(&mut identity, name.as_bytes());
        hash_identity_frame(&mut identity, &digest);
    }
    Ok(format!("sha256:{}", lowercase_hex(&identity.finalize())))
}

fn sha256_regular_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::InvalidPath,
            "failed to open native bundle identity input",
            error,
        )
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            io_error(
                WorkerResourceErrorCode::InvalidPath,
                "failed to hash native bundle identity input",
                error,
            )
        })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn hash_identity_frame(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)]);
        encoded.push(DIGITS[usize::from(byte & 0x0f)]);
    }
    String::from_utf8(encoded).expect("native bundle hexadecimal identity is UTF-8")
}

fn validate_native_bundle_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    directory: bool,
) -> Result<()> {
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || (!directory && metadata.nlink() != 1)
        || metadata.permissions().mode() & 0o7777 != 0o555
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!(
                "native inference bundle path is not exact and immutable: {}",
                path.display()
            ),
        ));
    }
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid && metadata.uid() != 0 {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!(
                "native inference bundle path has an unexpected owner: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

impl WorkerLaunchResources {
    pub fn render_devices(&self) -> &[RenderDeviceResource] {
        &self.render_devices
    }

    pub fn driver_manifests(&self) -> &[VulkanDriverManifestResource] {
        &self.driver_manifests
    }

    /// Exact paths that must be admitted read-only for delayed Vulkan driver
    /// loading: each selected manifest, its resolved driver library, and each
    /// admitted render device's canonical sysfs subtree.
    pub fn runtime_roots(&self) -> &[PathBuf] {
        &self.runtime_roots
    }

    pub const fn environment(&self) -> &WorkerLaunchEnvironment {
        &self.environment
    }
}

/// Discover host resources without initializing or linking a native runtime.
///
/// The only environment reads are `VK_DRIVER_FILES` and, when the former is
/// absent, `VK_ICD_FILENAMES`. An absent `/dev/dri` is a valid CPU-only result.
pub fn discover_worker_launch_resources() -> Result<WorkerLaunchResources> {
    let roots = DiscoveryRoots::system();
    let render_devices = discover_render_devices_with(&roots, &RealRenderNodeInspector)?;
    let driver_resources = discover_vulkan_driver_resources_from_values(
        std::env::var_os(VK_DRIVER_FILES),
        std::env::var_os(VK_ICD_FILENAMES),
    )?;
    let mut runtime_roots = driver_resources.runtime_roots;
    runtime_roots.extend(
        render_devices
            .iter()
            .map(|device| device.sysfs_device_root.clone()),
    );
    runtime_roots.sort();
    runtime_roots.dedup();
    Ok(WorkerLaunchResources {
        render_devices,
        driver_manifests: driver_resources.driver_manifests,
        runtime_roots,
        environment: driver_resources.environment,
    })
}

/// Enumerate only exact kernel-backed DRM render nodes and derive their sysfs
/// authority identities. No Vulkan/native runtime is loaded.
pub fn discover_render_devices() -> Result<Vec<RenderDeviceResource>> {
    discover_render_devices_with(&DiscoveryRoots::system(), &RealRenderNodeInspector)
}

/// Resolve the explicitly selected Vulkan manifests and driver libraries. This
/// function does not enumerate default Vulkan loader locations.
pub fn discover_vulkan_driver_resources() -> Result<VulkanDriverResources> {
    discover_vulkan_driver_resources_from_values(
        std::env::var_os(VK_DRIVER_FILES),
        std::env::var_os(VK_ICD_FILENAMES),
    )
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VulkanDriverResources {
    driver_manifests: Vec<VulkanDriverManifestResource>,
    runtime_roots: Vec<PathBuf>,
    environment: WorkerLaunchEnvironment,
}

impl VulkanDriverResources {
    pub fn driver_manifests(&self) -> &[VulkanDriverManifestResource] {
        &self.driver_manifests
    }

    pub fn runtime_roots(&self) -> &[PathBuf] {
        &self.runtime_roots
    }

    pub const fn environment(&self) -> &WorkerLaunchEnvironment {
        &self.environment
    }
}

fn discover_vulkan_driver_resources_from_values(
    driver_files: Option<OsString>,
    icd_filenames: Option<OsString>,
) -> Result<VulkanDriverResources> {
    // This is the Vulkan loader's defined precedence. Keeping only the
    // effective key also prevents an ignored legacy value from becoming child
    // process authority.
    let Some((environment_name, raw_value)) = driver_files
        .map(|value| (VK_DRIVER_FILES, value))
        .or_else(|| icd_filenames.map(|value| (VK_ICD_FILENAMES, value)))
    else {
        return Ok(VulkanDriverResources {
            driver_manifests: Vec::new(),
            runtime_roots: Vec::new(),
            environment: WorkerLaunchEnvironment::default(),
        });
    };

    let value = raw_value.into_string().map_err(|_| {
        WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidEnvironment,
            format!("{environment_name} must be bounded UTF-8"),
        )
    })?;
    if value.is_empty() || value.len() > MAX_ENVIRONMENT_VALUE_BYTES || value.contains('\0') {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidEnvironment,
            format!(
                "{environment_name} must be nonempty and at most {MAX_ENVIRONMENT_VALUE_BYTES} bytes"
            ),
        ));
    }

    let entries = value.split(':').collect::<Vec<_>>();
    if entries.is_empty() || entries.len() > MAX_DRIVER_MANIFESTS {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidEnvironment,
            format!(
                "{environment_name} must select between 1 and {MAX_DRIVER_MANIFESTS} manifests"
            ),
        ));
    }

    let mut unique_manifests = BTreeSet::new();
    let mut unique_runtime_roots = BTreeSet::new();
    let mut runtime_roots = Vec::with_capacity(entries.len() * 2);
    let mut manifests = Vec::with_capacity(entries.len());
    let mut normalized_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.is_empty() {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidEnvironment,
                format!("{environment_name} contains an empty manifest entry"),
            ));
        }
        let manifest_path = validate_vulkan_manifest_path(Path::new(entry))?;
        if manifest_path.extension() != Some(OsStr::new("json")) {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidManifest,
                "Vulkan driver manifest must have the .json suffix",
            ));
        }
        if !unique_manifests.insert(manifest_path.clone()) {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidEnvironment,
                format!("{environment_name} contains a duplicate manifest path"),
            ));
        }

        let manifest_bytes = read_regular_file_bounded(
            &manifest_path,
            MAX_MANIFEST_BYTES,
            "Vulkan driver manifest",
        )?;
        let manifest: DriverManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|error| {
                WorkerResourceError::new(
                    WorkerResourceErrorCode::InvalidManifest,
                    format!("Vulkan driver manifest is not valid bounded JSON: {error}"),
                )
            })?;
        let library_path = resolve_driver_library(&manifest_path, &manifest.icd.library_path)?;

        for path in [&manifest_path, &library_path] {
            if unique_runtime_roots.insert(path.clone()) {
                runtime_roots.push(path.clone());
            }
        }
        for dependency in elf::dependency_closure(std::slice::from_ref(&library_path), None)
            .map_err(|message| {
                WorkerResourceError::new(WorkerResourceErrorCode::InvalidManifest, message)
            })?
        {
            if unique_runtime_roots.insert(dependency.clone()) {
                runtime_roots.push(dependency);
            }
        }
        normalized_entries.push(manifest_path.to_string_lossy().into_owned());
        manifests.push(VulkanDriverManifestResource {
            manifest_path,
            library_path,
        });
    }

    // The deprecated override has the same manifest-list semantics. Normalize
    // it to the current name so the worker process needs only one allowlisted
    // Vulkan environment key.
    let mut environment = BTreeMap::new();
    environment.insert(VK_DRIVER_FILES.to_owned(), normalized_entries.join(":"));
    Ok(VulkanDriverResources {
        driver_manifests: manifests,
        runtime_roots,
        environment: WorkerLaunchEnvironment {
            values: environment,
        },
    })
}

fn validate_vulkan_manifest_path(path: &Path) -> Result<PathBuf> {
    const NIXOS_DRIVER_ALIAS: &str = "/run/opengl-driver/";
    if path
        .to_str()
        .is_some_and(|value| value.starts_with(NIXOS_DRIVER_ALIAS))
    {
        canonical_lexical_input(path, "Vulkan driver manifest")?;
        let resolved = fs::canonicalize(path).map_err(|error| {
            io_error(
                WorkerResourceErrorCode::InvalidPath,
                "failed to resolve NixOS Vulkan driver manifest alias",
                error,
            )
        })?;
        if !resolved.starts_with("/nix/store") {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                "NixOS Vulkan driver manifest alias must resolve into /nix/store",
            ));
        }
        return validate_regular_resource_path(&resolved, "Vulkan driver manifest");
    }
    validate_regular_resource_path(path, "Vulkan driver manifest")
}

fn canonical_lexical_input(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().is_empty()
        || path.as_os_str().as_bytes().len() > MAX_PATH_BYTES
        || path.as_os_str().as_bytes().contains(&0)
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!("{label} must be a canonical bounded absolute path"),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct DriverManifest {
    #[serde(rename = "ICD")]
    icd: DriverManifestIcd,
}

#[derive(Deserialize)]
struct DriverManifestIcd {
    library_path: String,
}

fn resolve_driver_library(manifest_path: &Path, library_path: &str) -> Result<PathBuf> {
    if library_path.is_empty()
        || library_path.len() > MAX_PATH_BYTES
        || library_path.as_bytes().contains(&0)
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidManifest,
            "Vulkan driver library_path is empty or exceeds its path bound",
        ));
    }
    let library_path = Path::new(library_path);
    let candidate = if library_path.is_absolute() {
        library_path.to_path_buf()
    } else {
        let components = library_path.components().collect::<Vec<_>>();
        if components.len() < 2
            || components
                .iter()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidManifest,
                "relative Vulkan library_path must be a canonical pathname; filename-only loader search is forbidden",
            ));
        }
        manifest_path
            .parent()
            .expect("validated absolute manifest has a parent")
            .join(library_path)
    };
    validate_regular_resource_path(&candidate, "Vulkan driver library")
}

#[derive(Clone)]
struct DiscoveryRoots {
    dev_dri: PathBuf,
    sys_root: PathBuf,
    sys_class_drm: PathBuf,
    sys_devices: PathBuf,
    sys_module: PathBuf,
}

impl DiscoveryRoots {
    fn system() -> Self {
        Self {
            dev_dri: PathBuf::from("/dev/dri"),
            sys_root: PathBuf::from("/sys"),
            sys_class_drm: PathBuf::from("/sys/class/drm"),
            sys_devices: PathBuf::from("/sys/devices"),
            sys_module: PathBuf::from("/sys/module"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DeviceNumber {
    major: u64,
    minor: u64,
}

trait RenderNodeInspector {
    fn inspect(&self, path: &Path) -> Result<DeviceNumber>;
}

struct RealRenderNodeInspector;

impl RenderNodeInspector for RealRenderNodeInspector {
    fn inspect(&self, path: &Path) -> Result<DeviceNumber> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            io_error(
                WorkerResourceErrorCode::InvalidRenderNode,
                "failed to inspect a DRM render node",
                error,
            )
        })?;
        if !metadata.file_type().is_char_device() {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidRenderNode,
                "DRM render node is not a character device",
            ));
        }
        Ok(DeviceNumber {
            major: u64::from(libc::major(metadata.rdev())),
            minor: u64::from(libc::minor(metadata.rdev())),
        })
    }
}

fn discover_render_devices_with(
    roots: &DiscoveryRoots,
    inspector: &dyn RenderNodeInspector,
) -> Result<Vec<RenderDeviceResource>> {
    let entries = match fs::read_dir(&roots.dev_dri) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io_error(
                WorkerResourceErrorCode::Io,
                "failed to enumerate /dev/dri",
                error,
            ));
        }
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            io_error(
                WorkerResourceErrorCode::Io,
                "failed to read a /dev/dri entry",
                error,
            )
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix("renderD") else {
            continue;
        };
        let Ok(suffix_number) = suffix.parse::<u64>() else {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidRenderNode,
                "render-node name has a non-decimal suffix",
            ));
        };
        if suffix.is_empty() || suffix_number.to_string() != suffix {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidRenderNode,
                "render-node name is not canonical decimal renderD<N>",
            ));
        }
        candidates.push((suffix_number, entry.path()));
    }
    candidates.sort_by_key(|(suffix, _)| *suffix);

    let mut devices = Vec::with_capacity(candidates.len());
    let mut device_paths = BTreeSet::new();
    for (suffix, path) in candidates {
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            io_error(
                WorkerResourceErrorCode::InvalidRenderNode,
                "failed to inspect a DRM render-node directory entry",
                error,
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidRenderNode,
                "DRM render node cannot be a symlink",
            ));
        }
        let canonical = canonical_lexical_absolute_path(&path, "DRM render node")?;
        if canonical.parent() != Some(roots.dev_dri.as_path())
            || canonical.file_name() != path.file_name()
            || !device_paths.insert(canonical.clone())
        {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidRenderNode,
                "DRM render node is aliased or outside the exact /dev/dri root",
            ));
        }
        let number = inspector.inspect(&canonical)?;
        if number.major != DRM_RENDER_MAJOR || number.minor != suffix {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidRenderNode,
                "DRM render node name and kernel device number disagree",
            ));
        }
        devices.push(derive_render_device_resource(
            roots, canonical, suffix, number,
        )?);
    }
    Ok(devices)
}

fn derive_render_device_resource(
    roots: &DiscoveryRoots,
    render_node: PathBuf,
    render_suffix: u64,
    number: DeviceNumber,
) -> Result<RenderDeviceResource> {
    let class_entry = roots.sys_class_drm.join(format!("renderD{render_suffix}"));
    let class_target = canonical_sysfs_target(
        &class_entry,
        &roots.sys_devices,
        "DRM render-node sysfs class entry",
    )?;
    let sysfs_number = read_trimmed_sysfs(&class_target.join("dev"))?;
    if sysfs_number != format!("{}:{}", number.major, number.minor) {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "DRM render node and sysfs device numbers disagree",
        ));
    }

    let physical_target = canonical_sysfs_target(
        &class_entry.join("device"),
        &roots.sys_devices,
        "DRM physical-device sysfs entry",
    )?;
    let uevent = parse_uevent(&read_bounded_sysfs(&physical_target.join("uevent"))?)?;
    let driver_target = canonical_sysfs_target(
        &physical_target.join("driver"),
        &roots.sys_root,
        "DRM driver sysfs entry",
    )?;
    if !driver_target.starts_with(roots.sys_root.join("bus")) {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "DRM driver sysfs target is outside /sys/bus",
        ));
    }
    let driver_name = driver_target
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| valid_sysfs_identifier(name))
        .ok_or_else(|| {
            WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "DRM driver sysfs target has no bounded kernel driver identifier",
            )
        })?;
    if uevent.get("DRIVER").map(String::as_str) != Some(driver_name) {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "physical-device uevent and driver sysfs target disagree",
        ));
    }

    let physical_device_id = derive_physical_device_id(roots, &physical_target, &uevent)?;
    let pci_device_id = uevent
        .get("PCI_ID")
        .map(|value| canonical_pci_id(value, "PCI_ID"))
        .transpose()?;
    let pci_subsystem_id = uevent
        .get("PCI_SUBSYS_ID")
        .map(|value| canonical_pci_id(value, "PCI_SUBSYS_ID"))
        .transpose()?;
    let (driver_build_id, driver_build_evidence_source) =
        derive_driver_build_id(roots, &driver_target, driver_name)?;
    let vram_total_path = optional_sysfs_pair_path(
        &physical_target.join("mem_info_vram_total"),
        &physical_target.join("mem_info_vram_used"),
    )?;
    let (vram_total_path, vram_used_path) = match vram_total_path {
        Some((total, used)) => (Some(total), Some(used)),
        None => (None, None),
    };
    Ok(RenderDeviceResource {
        render_node,
        sysfs_device_root: physical_target,
        physical_device_id,
        pci_device_id,
        pci_subsystem_id,
        driver_build_id,
        driver_build_evidence_source,
        vram_total_path,
        vram_used_path,
        device_major: number.major,
        device_minor: number.minor,
    })
    .and_then(|resource| {
        let _ = resource.memory_observation()?;
        Ok(resource)
    })
}

fn optional_sysfs_pair_path(first: &Path, second: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    let first_exists = first.try_exists().map_err(|error| {
        io_error(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "failed to inspect optional DRM memory evidence",
            error,
        )
    })?;
    let second_exists = second.try_exists().map_err(|error| {
        io_error(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "failed to inspect optional DRM memory evidence",
            error,
        )
    })?;
    if first_exists != second_exists {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "DRM driver exposes an incomplete VRAM counter pair",
        ));
    }
    if !first_exists {
        return Ok(None);
    }
    for path in [first, second] {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            io_error(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "failed to inspect DRM memory evidence",
                error,
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "DRM memory evidence cannot be a symlink",
            ));
        }
    }
    Ok(Some((first.to_path_buf(), second.to_path_buf())))
}

fn derive_physical_device_id(
    roots: &DiscoveryRoots,
    physical_target: &Path,
    uevent: &BTreeMap<String, String>,
) -> Result<String> {
    if let Some(slot) = uevent.get("PCI_SLOT_NAME") {
        let slot = canonical_pci_slot(slot)?;
        if physical_target.file_name().and_then(OsStr::to_str) != Some(slot.as_str()) {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "PCI_SLOT_NAME does not match the physical sysfs device",
            ));
        }
        return Ok(format!("pci:{slot}"));
    }

    let stable_hardware_evidence = uevent
        .get("MODALIAS")
        .or_else(|| uevent.get("OF_FULLNAME"))
        .ok_or_else(|| {
            WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "non-PCI DRM device has no MODALIAS or OF_FULLNAME identity evidence",
            )
        })?;
    let relative = physical_target
        .strip_prefix(&roots.sys_devices)
        .map_err(|_| {
            WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "physical DRM sysfs target is outside /sys/devices",
            )
        })?
        .as_os_str()
        .as_bytes();
    Ok(format!(
        "sysfs-sha256:{}",
        sha256_hex(&[relative, b"\0", stable_hardware_evidence.as_bytes()])
    ))
}

fn canonical_pci_id(value: &str, field: &str) -> Result<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 9
        || bytes[4] != b':'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| index != 4 && !byte.is_ascii_hexdigit())
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            format!("{field} is not a canonical vendor:device identity"),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn derive_driver_build_id(
    roots: &DiscoveryRoots,
    driver_target: &Path,
    driver_name: &str,
) -> Result<(String, DriverBuildEvidenceSource)> {
    let module_link = driver_target.join("module");
    match fs::canonicalize(&module_link) {
        Ok(module_target) => {
            if !module_target.starts_with(&roots.sys_module)
                || module_target.parent() != Some(roots.sys_module.as_path())
            {
                return Err(WorkerResourceError::new(
                    WorkerResourceErrorCode::MissingSysfsEvidence,
                    "DRM driver module target is outside the exact /sys/module root",
                ));
            }
            if let Some(note) =
                read_optional_bounded_sysfs(&module_target.join("notes/.note.gnu.build-id"))?
            {
                if note.is_empty() {
                    return Err(WorkerResourceError::new(
                        WorkerResourceErrorCode::MissingSysfsEvidence,
                        "kernel module build-id note is empty",
                    ));
                }
                return Ok((
                    format!(
                        "sha256:{}",
                        sha256_hex(&[b"module-build-id\0", driver_name.as_bytes(), b"\0", &note])
                    ),
                    DriverBuildEvidenceSource::KernelModuleBuildId,
                ));
            }

            let srcversion = read_optional_trimmed_sysfs(&module_target.join("srcversion"))?;
            let version = read_optional_trimmed_sysfs(&module_target.join("version"))?;
            if srcversion.is_none() && version.is_none() {
                return Err(WorkerResourceError::new(
                    WorkerResourceErrorCode::MissingSysfsEvidence,
                    "kernel module exposes neither a build-id nor bounded build attributes",
                ));
            }
            let module_name = module_target
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| {
                    WorkerResourceError::new(
                        WorkerResourceErrorCode::MissingSysfsEvidence,
                        "kernel module path is not UTF-8",
                    )
                })?;
            let evidence = format!(
                "driver={driver_name}\0module={module_name}\0srcversion={}\0version={}",
                srcversion.as_deref().unwrap_or(""),
                version.as_deref().unwrap_or("")
            );
            Ok((
                format!("sha256:{}", sha256_hex(&[evidence.as_bytes()])),
                DriverBuildEvidenceSource::KernelModuleAttributes,
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let kernel_note = read_bounded_sysfs(&roots.sys_root.join("kernel/notes"))?;
            if kernel_note.is_empty() {
                return Err(WorkerResourceError::new(
                    WorkerResourceErrorCode::MissingSysfsEvidence,
                    "built-in DRM driver has no kernel build-id evidence",
                ));
            }
            Ok((
                format!(
                    "sha256:{}",
                    sha256_hex(&[
                        b"kernel-build-id\0",
                        driver_name.as_bytes(),
                        b"\0",
                        &kernel_note
                    ])
                ),
                DriverBuildEvidenceSource::KernelBuildId,
            ))
        }
        Err(error) => Err(io_error(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "failed to resolve the DRM driver's kernel module",
            error,
        )),
    }
}

fn canonical_pci_slot(slot: &str) -> Result<String> {
    let bytes = slot.as_bytes();
    let valid = bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && bytes[..4].iter().all(u8::is_ascii_hexdigit)
        && bytes[5..7].iter().all(u8::is_ascii_hexdigit)
        && bytes[8..10].iter().all(u8::is_ascii_hexdigit)
        && bytes[11].is_ascii_hexdigit();
    if !valid {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "PCI_SLOT_NAME is not a canonical domain:bus:device.function identity",
        ));
    }
    let normalized = slot.to_ascii_lowercase();
    let device = u8::from_str_radix(&normalized[8..10], 16).unwrap_or(u8::MAX);
    let function = u8::from_str_radix(&normalized[11..12], 16).unwrap_or(u8::MAX);
    if normalized != slot || device > 0x1f || function > 7 {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "PCI_SLOT_NAME is not canonical lowercase PCI BDF syntax",
        ));
    }
    Ok(normalized)
}

fn canonical_sysfs_target(path: &Path, root: &Path, label: &str) -> Result<PathBuf> {
    let target = fs::canonicalize(path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            &format!("failed to resolve {label}"),
            error,
        )
    })?;
    if !target.is_absolute() || !target.starts_with(root) || target == root {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            format!("{label} escapes its kernel-owned sysfs root"),
        ));
    }
    Ok(target)
}

fn parse_uevent(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "physical-device uevent is not UTF-8",
        )
    })?;
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if fields.len() >= MAX_UEVENT_FIELDS {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "physical-device uevent exceeds its field bound",
            ));
        }
        let (name, value) = line.split_once('=').ok_or_else(|| {
            WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "physical-device uevent contains a malformed field",
            )
        })?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || value.is_empty()
            || value.as_bytes().contains(&0)
            || fields.insert(name.to_owned(), value.to_owned()).is_some()
        {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "physical-device uevent contains an invalid or duplicate field",
            ));
        }
    }
    Ok(fields)
}

fn valid_sysfs_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn validate_regular_resource_path(path: &Path, label: &str) -> Result<PathBuf> {
    let path = canonical_lexical_absolute_path(path, label)?;
    let metadata = fs::metadata(&path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::InvalidPath,
            &format!("failed to inspect {label}"),
            error,
        )
    })?;
    if !metadata.is_file() {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!("{label} must be an exact regular file"),
        ));
    }
    Ok(path)
}

fn canonical_lexical_absolute_path(path: &Path, label: &str) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().is_empty()
        || path.as_os_str().as_bytes().len() > MAX_PATH_BYTES
        || path.as_os_str().as_bytes().contains(&0)
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!("{label} must be a bounded absolute path"),
        ));
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(component) => normalized.push(component),
            _ => {
                return Err(WorkerResourceError::new(
                    WorkerResourceErrorCode::InvalidPath,
                    format!("{label} contains a dot, parent, or platform-prefix component"),
                ));
            }
        }
    }
    if normalized.as_os_str().as_bytes() != path.as_os_str().as_bytes()
        || normalized == Path::new("/")
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!("{label} is not a canonical absolute path"),
        ));
    }

    let mut prefix = PathBuf::from("/");
    for component in normalized.components().skip(1) {
        let Component::Normal(component) = component else {
            unreachable!("normalized path has only root and normal components");
        };
        prefix.push(component);
        let metadata = fs::symlink_metadata(&prefix).map_err(|error| {
            io_error(
                WorkerResourceErrorCode::InvalidPath,
                &format!("failed to inspect {label}"),
                error,
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(WorkerResourceError::new(
                WorkerResourceErrorCode::InvalidPath,
                format!("{label} cannot contain a symlink component"),
            ));
        }
    }
    let resolved = fs::canonicalize(&normalized).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::InvalidPath,
            &format!("failed to resolve {label}"),
            error,
        )
    })?;
    if resolved.as_os_str().as_bytes() != normalized.as_os_str().as_bytes() {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!("{label} does not resolve to its exact canonical path"),
        ));
    }
    Ok(normalized)
}

fn read_regular_file_bounded(path: &Path, maximum: usize, label: &str) -> Result<Vec<u8>> {
    let initial = fs::metadata(path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::Io,
            &format!("failed to inspect {label}"),
            error,
        )
    })?;
    if !initial.is_file() || initial.len() > maximum as u64 {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidManifest,
            format!("{label} is not regular or exceeds {maximum} bytes"),
        ));
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = options.open(path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::Io,
            &format!("failed to open {label}"),
            error,
        )
    })?;
    let opened = file.metadata().map_err(|error| {
        io_error(
            WorkerResourceErrorCode::Io,
            &format!("failed to inspect opened {label}"),
            error,
        )
    })?;
    if opened.dev() != initial.dev() || opened.ino() != initial.ino() || !opened.is_file() {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!("{label} changed while it was being pinned"),
        ));
    }
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(maximum));
    file.by_ref()
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            io_error(
                WorkerResourceErrorCode::Io,
                &format!("failed to read {label}"),
                error,
            )
        })?;
    if bytes.len() > maximum {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidManifest,
            format!("{label} exceeded {maximum} bytes while being read"),
        ));
    }
    let current = fs::metadata(path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::Io,
            &format!("failed to re-inspect {label}"),
            error,
        )
    })?;
    if current.dev() != opened.dev() || current.ino() != opened.ino() {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::InvalidPath,
            format!("{label} was replaced while it was being read"),
        ));
    }
    Ok(bytes)
}

fn read_bounded_sysfs(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "failed to read required sysfs evidence",
            error,
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "required sysfs evidence file is a symlink",
        ));
    }
    let mut file = File::open(path).map_err(|error| {
        io_error(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "failed to open required sysfs evidence",
            error,
        )
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SYSFS_ATTRIBUTE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            io_error(
                WorkerResourceErrorCode::MissingSysfsEvidence,
                "failed to read required sysfs evidence",
                error,
            )
        })?;
    if bytes.len() > MAX_SYSFS_ATTRIBUTE_BYTES {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "required sysfs evidence exceeds its byte bound",
        ));
    }
    Ok(bytes)
}

fn read_optional_bounded_sysfs(path: &Path) -> Result<Option<Vec<u8>>> {
    match read_bounded_sysfs(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error)
            if error.code() == WorkerResourceErrorCode::MissingSysfsEvidence
                && fs::symlink_metadata(path)
                    .is_err_and(|source| source.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn read_trimmed_sysfs(path: &Path) -> Result<String> {
    let bytes = read_bounded_sysfs(path)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "required sysfs evidence is not UTF-8",
        )
    })?;
    let trimmed = text.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty()
        || trimmed.contains('\n')
        || trimmed.contains('\r')
        || trimmed.contains('\0')
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "required sysfs evidence is not one bounded nonempty line",
        ));
    }
    Ok(trimmed.to_owned())
}

fn read_optional_trimmed_sysfs(path: &Path) -> Result<Option<String>> {
    if !path.try_exists().map_err(|error| {
        io_error(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            "failed to inspect optional sysfs evidence",
            error,
        )
    })? {
        return Ok(None);
    }
    read_trimmed_sysfs(path).map(Some)
}

fn read_canonical_u64_sysfs(path: &Path, label: &str) -> Result<u64> {
    let value = read_trimmed_sysfs(path)?;
    if !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            format!("{label} is not a canonical decimal byte count"),
        ));
    }
    value.parse::<u64>().map_err(|_| {
        WorkerResourceError::new(
            WorkerResourceErrorCode::MissingSysfsEvidence,
            format!("{label} exceeds the supported byte range"),
        )
    })
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn io_error(code: WorkerResourceErrorCode, action: &str, error: io::Error) -> WorkerResourceError {
    WorkerResourceError::new(code, format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agl-worker-resources-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create isolated fixture root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            make_tree_owner_writable(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_tree_owner_writable(path: &Path) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_tree_owner_writable(&entry.path());
                }
            }
        } else {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
        }
    }

    struct FixtureInspector {
        major: u64,
        minor: u64,
    }

    impl RenderNodeInspector for FixtureInspector {
        fn inspect(&self, _path: &Path) -> Result<DeviceNumber> {
            Ok(DeviceNumber {
                major: self.major,
                minor: self.minor,
            })
        }
    }

    fn driver_fixture(library_path: &str) -> (TestDirectory, PathBuf, PathBuf) {
        let fixture = TestDirectory::new();
        let manifest_dir = fixture.path().join("share/vulkan/icd.d");
        let library_dir = manifest_dir.join("lib");
        fs::create_dir_all(&manifest_dir).expect("create manifest directory");
        fs::create_dir_all(&library_dir).expect("create library directory");
        let manifest = manifest_dir.join("test_icd.json");
        let library = library_dir.join("libtest_icd.so");
        let mut elf = vec![0_u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
        fs::write(&library, elf).expect("write driver fixture");
        fs::write(
            &manifest,
            format!(r#"{{"file_format_version":"1.0.1","ICD":{{"library_path":"{library_path}","api_version":"1.3.0"}}}}"#),
        )
        .expect("write manifest fixture");
        (fixture, manifest, library)
    }

    fn native_bundle_fixture() -> (TestDirectory, PathBuf) {
        let fixture = TestDirectory::new();
        let bundle_base = fixture.path().join(NATIVE_BUNDLE_BASE_DIRECTORY);
        let bundle = bundle_base.join("manifest-staging");
        fs::create_dir_all(&bundle).expect("create native bundle fixture");

        let library_source = fixture.path().join("fixture-library.rs");
        let library = fixture.path().join("libfixture.so");
        fs::write(
            &library_source,
            "#[unsafe(no_mangle)] pub extern \"C\" fn agl_fixture() -> u32 { 42 }\n",
        )
        .expect("write native library fixture source");
        let compiler = option_env!("RUSTC").unwrap_or("rustc");
        let status = Command::new(compiler)
            .arg("--edition=2024")
            .arg("--crate-type=cdylib")
            .arg(&library_source)
            .arg("-o")
            .arg(&library)
            .status()
            .expect("compile native library fixture");
        assert!(status.success(), "native library fixture must compile");
        for name in REQUIRED_NATIVE_LIBRARIES {
            fs::copy(&library, bundle.join(name)).expect("copy core native ELF fixture");
        }
        fs::copy(&library, bundle.join("libggml-cpu-fixture.so")).expect("copy CPU plugin fixture");
        fs::write(
            bundle.join("libggml-vulkan.so"),
            b"deliberately unavailable Vulkan loader fixture",
        )
        .expect("write malformed Vulkan plugin fixture");

        let names = fs::read_dir(&bundle)
            .expect("list unsealed native bundle fixture")
            .map(|entry| {
                entry
                    .expect("read unsealed native bundle fixture entry")
                    .file_name()
                    .into_string()
                    .expect("UTF-8 native bundle fixture name")
            })
            .collect::<BTreeSet<_>>();
        let identity = native_bundle_identity(&bundle, &names).expect("hash native bundle fixture");
        let digest = identity
            .strip_prefix("sha256:")
            .expect("fixture identity is sha256");
        let bundle = bundle_base.join(format!("{NATIVE_BUNDLE_DIRECTORY_PREFIX}{digest}"));
        fs::rename(bundle_base.join("manifest-staging"), &bundle)
            .expect("publish content-addressed native bundle fixture");
        for entry in fs::read_dir(&bundle).expect("list native bundle fixture") {
            fs::set_permissions(
                entry.expect("read native bundle fixture entry").path(),
                fs::Permissions::from_mode(0o555),
            )
            .expect("seal native bundle fixture entry");
        }
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("seal native bundle fixture directory");

        let worker = fixture.path().join("agl-inference-worker");
        compile_worker_fixture(
            fixture.path(),
            &worker,
            &[format!(
                "$ORIGIN/{NATIVE_BUNDLE_BASE_DIRECTORY}/{NATIVE_BUNDLE_DIRECTORY_PREFIX}{digest}"
            )],
        );
        (fixture, worker)
    }

    fn compile_worker_fixture(root: &Path, worker: &Path, native_rpaths: &[String]) {
        let source = root.join(format!(
            "{}.rs",
            worker
                .file_name()
                .and_then(|name| name.to_str())
                .expect("worker fixture name is UTF-8")
        ));
        fs::write(&source, "fn main() {}\n").expect("write worker fixture source");
        let compiler = option_env!("RUSTC").unwrap_or("rustc");
        let mut command = Command::new(compiler);
        command.arg("--edition=2024");
        for rpath in native_rpaths {
            command
                .arg("-C")
                .arg(format!("link-arg=-Wl,-rpath,{rpath}"));
        }
        let status = command
            .arg("-C")
            .arg(format!(
                "link-arg=-Wl,-rpath,{}",
                loaded_library_directory("libgcc_s.so.1").display()
            ))
            .arg(&source)
            .arg("-o")
            .arg(worker)
            .status()
            .expect("compile worker ELF fixture");
        assert!(status.success(), "worker ELF fixture must compile");
    }

    fn add_native_bundle_variant(root: &Path, source: &Path, label: &str) -> PathBuf {
        let base = root.join(NATIVE_BUNDLE_BASE_DIRECTORY);
        let staging = base.join(format!("variant-{label}-staging"));
        fs::create_dir(&staging).expect("create second native bundle staging leaf");
        for entry in fs::read_dir(source).expect("list source native bundle variant") {
            let entry = entry.expect("read source native bundle variant entry");
            let source_name = entry
                .file_name()
                .into_string()
                .expect("source native bundle name is UTF-8");
            let destination_name = if source_name.starts_with("libggml-cpu-") {
                format!("libggml-cpu-{label}.so")
            } else {
                source_name
            };
            fs::copy(entry.path(), staging.join(destination_name))
                .expect("copy second native bundle variant file");
        }
        let names = fs::read_dir(&staging)
            .expect("list second native bundle variant")
            .map(|entry| {
                entry
                    .expect("read second native bundle variant entry")
                    .file_name()
                    .into_string()
                    .expect("second native bundle name is UTF-8")
            })
            .collect::<BTreeSet<_>>();
        let identity =
            native_bundle_identity(&staging, &names).expect("hash second native bundle variant");
        let digest = identity
            .strip_prefix("sha256:")
            .expect("second variant identity is sha256");
        let destination = base.join(format!("{NATIVE_BUNDLE_DIRECTORY_PREFIX}{digest}"));
        fs::rename(&staging, &destination).expect("publish second native bundle variant");
        for entry in fs::read_dir(&destination).expect("list published second variant") {
            fs::set_permissions(
                entry.expect("read published second variant entry").path(),
                fs::Permissions::from_mode(0o555),
            )
            .expect("seal second native bundle variant file");
        }
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o555))
            .expect("seal second native bundle variant leaf");
        destination
    }

    fn loaded_library_directory(file_name: &str) -> PathBuf {
        let maps = fs::read_to_string("/proc/self/maps").expect("read test process memory map");
        maps.lines()
            .filter_map(|line| line.split_whitespace().last())
            .map(Path::new)
            .find(|path| path.file_name().is_some_and(|name| name == file_name))
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| panic!("test process did not map {file_name}"))
    }

    fn sysfs_fixture() -> (TestDirectory, DiscoveryRoots, PathBuf) {
        let fixture = TestDirectory::new();
        let dev_dri = fixture.path().join("dev/dri");
        let sys_root = fixture.path().join("sys");
        let sys_class_drm = sys_root.join("class/drm");
        let sys_devices = sys_root.join("devices");
        let sys_module = sys_root.join("module");
        fs::create_dir_all(&dev_dri).expect("create fake dri root");
        fs::create_dir_all(&sys_class_drm).expect("create fake class root");
        fs::create_dir_all(&sys_devices).expect("create fake devices root");
        fs::create_dir_all(&sys_module).expect("create fake module root");

        let render_node = dev_dri.join("renderD128");
        fs::write(&render_node, b"fake character device").expect("write fake render node");
        let physical = sys_devices.join("pci0000:00/0000:03:00.0");
        let drm_entry = physical.join("drm/renderD128");
        fs::create_dir_all(&drm_entry).expect("create fake DRM sysfs entry");
        fs::write(drm_entry.join("dev"), b"226:128\n").expect("write device number");
        fs::write(
            physical.join("uevent"),
            b"DRIVER=amdgpu\nPCI_ID=1002:744C\nPCI_SUBSYS_ID=1DA2:471E\nPCI_SLOT_NAME=0000:03:00.0\nMODALIAS=pci:fixture\n",
        )
        .expect("write physical-device uevent");
        fs::write(physical.join("product_name"), b"ignored display name\n")
            .expect("write ignored display-name fixture");
        fs::write(physical.join("mem_info_vram_total"), b"24560\n").expect("write fake VRAM total");
        fs::write(physical.join("mem_info_vram_used"), b"4096\n").expect("write fake VRAM used");

        let class_entry = sys_class_drm.join("renderD128");
        symlink(&drm_entry, &class_entry).expect("link fake class entry");
        symlink(&physical, drm_entry.join("device")).expect("link physical device");

        let driver = sys_root.join("bus/pci/drivers/amdgpu");
        fs::create_dir_all(&driver).expect("create fake driver root");
        symlink(&driver, physical.join("driver")).expect("link fake driver");
        let module = sys_module.join("amdgpu");
        fs::create_dir_all(module.join("notes")).expect("create fake module notes");
        fs::write(
            module.join("notes/.note.gnu.build-id"),
            b"exact fake module build id",
        )
        .expect("write fake module build id");
        symlink(&module, driver.join("module")).expect("link fake module");

        (
            fixture,
            DiscoveryRoots {
                dev_dri,
                sys_root,
                sys_class_drm,
                sys_devices,
                sys_module,
            },
            render_node,
        )
    }

    #[test]
    fn explicit_driver_manifest_resolves_only_exact_runtime_files() {
        let (_fixture, manifest, library) = driver_fixture("lib/libtest_icd.so");
        let resources = discover_vulkan_driver_resources_from_values(
            Some(manifest.as_os_str().to_owned()),
            None,
        )
        .expect("resolve explicit Vulkan resources");
        assert_eq!(
            resources.environment.values(),
            &BTreeMap::from([(
                VK_DRIVER_FILES.to_owned(),
                manifest.to_string_lossy().into_owned()
            )])
        );
        assert_eq!(resources.driver_manifests.len(), 1);
        assert_eq!(resources.driver_manifests[0].manifest_path(), manifest);
        assert_eq!(resources.driver_manifests[0].library_path(), library);
        assert_eq!(resources.runtime_roots, vec![manifest, library]);
    }

    #[test]
    fn newer_driver_override_has_exact_precedence_and_legacy_is_not_forwarded() {
        let (_fixture, manifest, _library) = driver_fixture("lib/libtest_icd.so");
        let resources = discover_vulkan_driver_resources_from_values(
            Some(manifest.as_os_str().to_owned()),
            Some(OsString::from("relative-or-ignored.json")),
        )
        .expect("ignore deprecated value when newer override exists");
        assert_eq!(resources.environment.values().len(), 1);
        assert!(resources.environment.values().contains_key(VK_DRIVER_FILES));
        assert!(
            !resources
                .environment
                .values()
                .contains_key(VK_ICD_FILENAMES)
        );
    }

    #[test]
    fn legacy_override_is_parsed_but_normalized_to_the_current_worker_key() {
        let (_fixture, manifest, _library) = driver_fixture("lib/libtest_icd.so");
        let resources = discover_vulkan_driver_resources_from_values(
            None,
            Some(manifest.as_os_str().to_owned()),
        )
        .expect("parse legacy explicit override");
        assert_eq!(
            resources.environment.values(),
            &BTreeMap::from([(
                VK_DRIVER_FILES.to_owned(),
                manifest.to_string_lossy().into_owned()
            )])
        );
        assert_eq!(
            resources.environment.to_process_environment(),
            BTreeMap::from([(VK_DRIVER_FILES.to_owned(), manifest.into_os_string())])
        );
    }

    #[test]
    fn absent_override_does_not_scan_default_locations() {
        let resources = discover_vulkan_driver_resources_from_values(None, None)
            .expect("CPU-only resource discovery");
        assert!(resources.driver_manifests.is_empty());
        assert!(resources.runtime_roots.is_empty());
        assert!(resources.environment.is_empty());
    }

    #[test]
    fn cpu_bundle_admission_does_not_require_a_loadable_vulkan_plugin() {
        let (_fixture, worker) = native_bundle_fixture();
        let cpu = discover_native_bundle_for_worker(&worker, &worker, false)
            .expect("CPU admission must ignore unavailable Vulkan runtime dependencies");
        assert!(cpu.identity().starts_with("sha256:"));
        assert!(
            cpu.external_dependencies()
                .iter()
                .all(|path| !path.starts_with(cpu.directory()))
        );
        assert!(cpu.external_dependencies().iter().all(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libggml"))
        }));

        let error = discover_native_bundle_for_worker(&worker, &worker, true)
            .expect_err("GPU admission must validate the selected Vulkan plugin closure");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidPath);
    }

    #[test]
    fn embedded_bundle_variants_coexist_and_never_substitute() {
        let (fixture, first_worker) = native_bundle_fixture();
        let first = discover_native_bundle_for_worker(&first_worker, &first_worker, false)
            .expect("resolve first embedded native bundle");
        let first_directory = first.directory().to_path_buf();
        let second_directory = add_native_bundle_variant(fixture.path(), &first_directory, "other");
        let second_digest = second_directory
            .file_name()
            .and_then(|name| name.to_str())
            .expect("second leaf name is UTF-8");
        let second_worker = fixture.path().join("agl-inference-worker-other");
        compile_worker_fixture(
            fixture.path(),
            &second_worker,
            &[format!(
                "$ORIGIN/{NATIVE_BUNDLE_BASE_DIRECTORY}/{second_digest}"
            )],
        );

        let first_again = discover_native_bundle_for_worker(&first_worker, &first_worker, false)
            .expect("first worker keeps its embedded leaf with another valid leaf present");
        let second = discover_native_bundle_for_worker(&second_worker, &second_worker, false)
            .expect("second worker resolves only its embedded leaf");
        assert_eq!(first_again.directory(), first_directory);
        assert_eq!(second.directory(), second_directory);
        assert_ne!(first_again.identity(), second.identity());

        make_tree_owner_writable(&first_directory);
        fs::remove_dir_all(&first_directory).expect("remove first worker's selected leaf");
        let error = discover_native_bundle_for_worker(&first_worker, &first_worker, false)
            .expect_err("a valid other leaf must never substitute for the embedded leaf");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidPath);
        assert_eq!(
            discover_native_bundle_for_worker(&second_worker, &second_worker, false)
                .expect("other worker remains valid after first leaf removal")
                .directory(),
            second_directory
        );
    }

    #[test]
    fn embedded_bundle_runpath_is_exact_and_lowercase() {
        let (fixture, worker) = native_bundle_fixture();
        let selected = discover_native_bundle_for_worker(&worker, &worker, false)
            .expect("resolve exact fixture worker leaf")
            .directory()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("fixture leaf name is UTF-8")
            .to_owned();
        let cases = [
            ("missing", Vec::new()),
            (
                "multiple",
                vec![
                    format!("$ORIGIN/{NATIVE_BUNDLE_BASE_DIRECTORY}/{selected}"),
                    format!(
                        "$ORIGIN/{NATIVE_BUNDLE_BASE_DIRECTORY}/sha256-{}",
                        "0".repeat(64)
                    ),
                ],
            ),
            (
                "uppercase",
                vec![format!(
                    "$ORIGIN/{NATIVE_BUNDLE_BASE_DIRECTORY}/sha256-{}",
                    "A".repeat(64)
                )],
            ),
            (
                "traversal",
                vec![format!(
                    "$ORIGIN/{NATIVE_BUNDLE_BASE_DIRECTORY}/../{NATIVE_BUNDLE_BASE_DIRECTORY}/{selected}"
                )],
            ),
        ];
        for (label, rpaths) in cases {
            let candidate = fixture.path().join(format!("worker-{label}"));
            compile_worker_fixture(fixture.path(), &candidate, &rpaths);
            let error = discover_native_bundle_for_worker(&candidate, &candidate, false)
                .expect_err("unsafe or ambiguous native bundle RUNPATH must fail");
            assert_eq!(
                error.code(),
                WorkerResourceErrorCode::InvalidPath,
                "{label}"
            );
        }
    }

    #[test]
    fn host_rejects_bundle_content_and_metadata_substitution() {
        let (_fixture, worker) = native_bundle_fixture();
        let bundle = discover_native_bundle_for_worker(&worker, &worker, false)
            .expect("resolve exact fixture worker leaf")
            .directory()
            .to_path_buf();
        let assert_rejected = || {
            let error = discover_native_bundle_for_worker(&worker, &worker, false)
                .expect_err("substituted native bundle must fail closed");
            assert_eq!(error.code(), WorkerResourceErrorCode::InvalidPath);
        };

        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make selected leaf writable");
        assert_rejected();
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("restore selected leaf mode");

        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make selected leaf writable for extra-file fixture");
        let extra = bundle.join("unexpected.so");
        fs::write(&extra, b"unexpected native bundle entry").expect("write extra bundle file");
        fs::set_permissions(&extra, fs::Permissions::from_mode(0o555))
            .expect("seal extra bundle file");
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("seal selected leaf with extra file");
        assert_rejected();
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make selected leaf writable to remove extra file");
        fs::remove_file(&extra).expect("remove extra bundle file");
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("restore selected leaf after extra-file fixture");

        let file = bundle.join(REQUIRED_NATIVE_LIBRARIES[0]);
        let original = fs::read(&file).expect("read original native bundle bytes");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755))
            .expect("make native bundle file writable");
        fs::write(&file, b"digest substitution").expect("substitute native bundle bytes");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o555))
            .expect("reseal digest-substituted bundle file");
        assert_rejected();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755))
            .expect("make native bundle file writable for restoration");
        fs::write(&file, original).expect("restore native bundle bytes");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o555))
            .expect("reseal restored native bundle file");

        let outside = bundle
            .parent()
            .expect("bundle has a base")
            .join("linked.so");
        fs::hard_link(&file, &outside).expect("create second native bundle file link");
        assert_rejected();
        fs::remove_file(&outside).expect("remove second native bundle file link");

        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make selected leaf writable for symlink fixture");
        let outside = bundle
            .parent()
            .expect("bundle has a base")
            .join("target.so");
        fs::rename(&file, &outside).expect("move selected native bundle file");
        symlink(&outside, &file).expect("substitute selected native bundle file with symlink");
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("reseal symlink-substituted native bundle");
        assert_rejected();
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make selected leaf writable to restore symlink fixture");
        fs::remove_file(&file).expect("remove substituted native bundle symlink");
        fs::rename(&outside, &file).expect("restore exact native bundle file");
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("reseal restored native bundle");

        let base = bundle.parent().expect("bundle has a base");
        let moved = base.join("selected-leaf-target");
        fs::rename(&bundle, &moved).expect("move selected native bundle leaf");
        symlink(&moved, &bundle).expect("substitute selected native bundle leaf with symlink");
        assert_rejected();
    }

    #[test]
    fn nixos_driver_alias_normalizes_to_an_exact_store_closure_when_present() {
        let alias = Path::new("/run/opengl-driver/share/vulkan/icd.d/radeon_icd.x86_64.json");
        if !alias.exists() {
            return;
        }
        let resources =
            discover_vulkan_driver_resources_from_values(Some(alias.as_os_str().to_owned()), None)
                .expect("resolve the real NixOS Vulkan ICD closure");
        let normalized = resources
            .environment()
            .values()
            .get(VK_DRIVER_FILES)
            .expect("normalized Vulkan override");
        assert!(normalized.starts_with("/nix/store/"));
        assert!(!normalized.starts_with("/run/opengl-driver/"));
        assert!(resources.runtime_roots().len() > 2);
        assert!(
            resources
                .runtime_roots()
                .iter()
                .all(|path| !path.starts_with("/run/opengl-driver"))
        );
    }

    #[test]
    fn manifest_list_is_strictly_bounded_absolute_and_file_only() {
        let fixture = TestDirectory::new();
        let relative = discover_vulkan_driver_resources_from_values(
            Some(OsString::from("relative.json")),
            None,
        )
        .expect_err("relative manifest must fail");
        assert_eq!(relative.code(), WorkerResourceErrorCode::InvalidPath);

        let empty =
            discover_vulkan_driver_resources_from_values(Some(OsString::from(":/one.json")), None)
                .expect_err("empty list member must fail");
        assert_eq!(empty.code(), WorkerResourceErrorCode::InvalidEnvironment);

        let too_many = std::iter::repeat_n("/manifest.json", MAX_DRIVER_MANIFESTS + 1)
            .collect::<Vec<_>>()
            .join(":");
        let error =
            discover_vulkan_driver_resources_from_values(Some(OsString::from(too_many)), None)
                .expect_err("manifest count bound must be enforced before path access");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidEnvironment);

        let error = discover_vulkan_driver_resources_from_values(
            Some(OsString::from("x".repeat(MAX_ENVIRONMENT_VALUE_BYTES + 1))),
            None,
        )
        .expect_err("environment byte bound must be enforced before path access");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidEnvironment);

        let directory = discover_vulkan_driver_resources_from_values(
            Some(fixture.path().as_os_str().to_owned()),
            None,
        )
        .expect_err("manifest directory must fail");
        assert_eq!(directory.code(), WorkerResourceErrorCode::InvalidPath);

        let non_utf8 = discover_vulkan_driver_resources_from_values(
            Some(OsString::from_vec(vec![0xff])),
            None,
        )
        .expect_err("non-UTF-8 manifest override must fail");
        assert_eq!(non_utf8.code(), WorkerResourceErrorCode::InvalidEnvironment);
    }

    #[test]
    fn noncanonical_and_oversized_manifests_are_rejected() {
        let (fixture, manifest, _library) = driver_fixture("lib/libtest_icd.so");
        let noncanonical = format!(
            "{}/./share/vulkan/icd.d/test_icd.json",
            fixture.path().display()
        );
        let error =
            discover_vulkan_driver_resources_from_values(Some(OsString::from(noncanonical)), None)
                .expect_err("noncanonical manifest path must fail");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidPath);

        fs::write(&manifest, vec![b' '; MAX_MANIFEST_BYTES + 1])
            .expect("write oversized manifest fixture");
        let error =
            discover_vulkan_driver_resources_from_values(Some(manifest.into_os_string()), None)
                .expect_err("oversized manifest must fail before JSON parsing");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidManifest);
    }

    #[test]
    fn symlinked_manifest_and_driver_library_are_rejected() {
        let (fixture, manifest, library) = driver_fixture("lib/libtest_icd.so");
        let manifest_link = fixture.path().join("manifest-link.json");
        symlink(&manifest, &manifest_link).expect("link manifest fixture");
        let error = discover_vulkan_driver_resources_from_values(
            Some(manifest_link.into_os_string()),
            None,
        )
        .expect_err("symlinked manifest must fail");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidPath);

        let library_link = fixture.path().join("share/vulkan/icd.d/lib/link.so");
        symlink(&library, &library_link).expect("link library fixture");
        fs::write(&manifest, r#"{"ICD":{"library_path":"lib/link.so"}}"#)
            .expect("rewrite manifest for linked library");
        let error =
            discover_vulkan_driver_resources_from_values(Some(manifest.into_os_string()), None)
                .expect_err("symlinked driver library must fail");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidPath);
    }

    #[test]
    fn filename_only_driver_library_cannot_trigger_loader_path_search() {
        let (_fixture, manifest, _library) = driver_fixture("libtest_icd.so");
        let error =
            discover_vulkan_driver_resources_from_values(Some(manifest.into_os_string()), None)
                .expect_err("filename-only library must fail closed");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidManifest);
    }

    #[test]
    fn render_node_uses_kernel_numbers_and_non_display_sysfs_identity() {
        let (_fixture, roots, render_node) = sysfs_fixture();
        let devices = discover_render_devices_with(
            &roots,
            &FixtureInspector {
                major: 226,
                minor: 128,
            },
        )
        .expect("derive render-node authority");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].render_node(), render_node);
        assert_eq!(devices[0].physical_device_id(), "pci:0000:03:00.0");
        assert_eq!(devices[0].pci_device_id(), Some("1002:744c"));
        assert_eq!(devices[0].pci_subsystem_id(), Some("1da2:471e"));
        assert_eq!(devices[0].device_major(), 226);
        assert_eq!(devices[0].device_minor(), 128);
        assert_eq!(
            devices[0].driver_build_evidence_source(),
            DriverBuildEvidenceSource::KernelModuleBuildId
        );
        assert!(devices[0].driver_build_id().starts_with("sha256:"));
        assert_eq!(
            devices[0].memory_observation().unwrap(),
            Some(DeviceMemoryObservation {
                total_bytes: 24_560,
                used_bytes: 4_096,
                available_bytes: 20_464,
            })
        );

        fs::write(
            roots
                .sys_devices
                .join("pci0000:00/0000:03:00.0/product_name"),
            b"a completely different display name\n",
        )
        .expect("change ignored display name");
        let second = discover_render_devices_with(
            &roots,
            &FixtureInspector {
                major: 226,
                minor: 128,
            },
        )
        .expect("rederive render-node authority");
        assert_eq!(devices, second);
    }

    #[test]
    fn malformed_or_incomplete_vram_counters_fail_closed() {
        let (_fixture, roots, _render_node) = sysfs_fixture();
        let physical = roots.sys_devices.join("pci0000:00/0000:03:00.0");
        fs::write(physical.join("mem_info_vram_used"), b"24561\n")
            .expect("write impossible used count");
        let error = discover_render_devices_with(
            &roots,
            &FixtureInspector {
                major: 226,
                minor: 128,
            },
        )
        .expect_err("used above total must fail");
        assert_eq!(error.code(), WorkerResourceErrorCode::MissingSysfsEvidence);

        fs::write(physical.join("mem_info_vram_used"), b"0001\n")
            .expect("write noncanonical count");
        let error = discover_render_devices_with(
            &roots,
            &FixtureInspector {
                major: 226,
                minor: 128,
            },
        )
        .expect_err("noncanonical byte count must fail");
        assert_eq!(error.code(), WorkerResourceErrorCode::MissingSysfsEvidence);

        fs::remove_file(physical.join("mem_info_vram_used")).expect("remove used counter");
        let error = discover_render_devices_with(
            &roots,
            &FixtureInspector {
                major: 226,
                minor: 128,
            },
        )
        .expect_err("incomplete counter pair must fail");
        assert_eq!(error.code(), WorkerResourceErrorCode::MissingSysfsEvidence);
    }

    #[test]
    fn render_node_rejects_symlink_wrong_major_and_sysfs_mismatch() {
        let (fixture, roots, render_node) = sysfs_fixture();
        let regular_file = RealRenderNodeInspector
            .inspect(&render_node)
            .expect_err("production inspector must reject a fixture regular file");
        assert_eq!(
            regular_file.code(),
            WorkerResourceErrorCode::InvalidRenderNode
        );
        let wrong_major = discover_render_devices_with(
            &roots,
            &FixtureInspector {
                major: 1,
                minor: 128,
            },
        )
        .expect_err("wrong DRM major must fail");
        assert_eq!(
            wrong_major.code(),
            WorkerResourceErrorCode::InvalidRenderNode
        );

        fs::write(
            roots
                .sys_devices
                .join("pci0000:00/0000:03:00.0/drm/renderD128/dev"),
            b"226:129\n",
        )
        .expect("corrupt sysfs device-number fixture");
        let mismatch = discover_render_devices_with(
            &roots,
            &FixtureInspector {
                major: 226,
                minor: 128,
            },
        )
        .expect_err("sysfs mismatch must fail");
        assert_eq!(
            mismatch.code(),
            WorkerResourceErrorCode::MissingSysfsEvidence
        );

        drop(fixture);
        let symlink_fixture = TestDirectory::new();
        let dev_dri = symlink_fixture.path().join("dev/dri");
        fs::create_dir_all(&dev_dri).expect("create symlink test dri root");
        let target = symlink_fixture.path().join("target");
        fs::write(&target, b"target").expect("write symlink target");
        symlink(&target, dev_dri.join("renderD128")).expect("link fake render node");
        let symlink_roots = DiscoveryRoots { dev_dri, ..roots };
        let error = discover_render_devices_with(
            &symlink_roots,
            &FixtureInspector {
                major: 226,
                minor: 128,
            },
        )
        .expect_err("symlink render node must fail");
        assert_eq!(error.code(), WorkerResourceErrorCode::InvalidRenderNode);
        let _ = render_node;
    }

    #[test]
    fn missing_dev_dri_is_a_valid_cpu_only_inventory() {
        let fixture = TestDirectory::new();
        let roots = DiscoveryRoots {
            dev_dri: fixture.path().join("absent/dev/dri"),
            sys_root: fixture.path().join("sys"),
            sys_class_drm: fixture.path().join("sys/class/drm"),
            sys_devices: fixture.path().join("sys/devices"),
            sys_module: fixture.path().join("sys/module"),
        };
        assert!(
            discover_render_devices_with(&roots, &RealRenderNodeInspector)
                .expect("missing /dev/dri must be CPU-only")
                .is_empty()
        );
    }
}
