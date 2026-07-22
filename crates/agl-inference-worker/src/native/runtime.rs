use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use agl_config::{KvCacheType, MtpRuntimeConfig, ResolvedInferenceConfig};
use agl_content::Content;
use agl_inference::worker_protocol::{AllocationReceipt, WORKER_BUILD_ID, WorkerFailureCode};
use agl_inference::{
    InferenceDeviceInfo, InferenceDeviceKind, InferenceJob, ModelGeneration, ModelRuntime,
    ResolvedContentPart, ResolvedModelContent, RuntimeFailure, RuntimeOperation,
};
use agl_oven::RenderedModelRequest;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::context_slot::{LlamaCppContextSlot, LlamaCppGenerationRequest};
use super::ffi;
use super::generation::LlamaCppGenerationControl;
use super::model::LlamaCppModel;
use crate::service::WorkerServiceRuntime;

static LLAMA_BACKEND: OnceLock<()> = OnceLock::new();
static LLAMA_LOGS: Mutex<NativeLogState> = Mutex::new(NativeLogState { active: None });
const MAX_NATIVE_LOG_BYTES: usize = 4 * 1024 * 1024;
const NATIVE_LOG_TRUNCATION_MARKER: &str = "\n[agentlibre native log truncated]\n";
const CPU_PHYSICAL_DEVICE_ID: &str = "cpu";

struct NativeLogState {
    active: Option<NativeLogBuffer>,
}

#[derive(Default)]
struct NativeLogBuffer {
    content: String,
    truncated: bool,
}

pub(crate) struct NativeLogCapture {
    active: bool,
}

pub struct LlamaCppModelRuntime {
    native_library_dir: PathBuf,
}

impl LlamaCppModelRuntime {
    pub fn new() -> Self {
        #[cfg(test)]
        {
            Self::with_native_library_dir(ffi::library_dir())
        }
        #[cfg(not(test))]
        {
            let executable = std::env::current_exe().expect("resolve exact inference worker");
            let parent = executable
                .parent()
                .expect("exact inference worker has a sibling directory");
            Self::with_native_library_dir(parent.join(env!("AGL_INFERENCE_NATIVE_RELATIVE_DIR")))
        }
    }

    pub fn with_native_library_dir(path: impl Into<PathBuf>) -> Self {
        Self {
            native_library_dir: path.into(),
        }
    }
}

impl Default for LlamaCppModelRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlamaCppDeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    Accelerator,
    Metadata,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlamaCppDeviceInfo {
    /// Stable physical identity reported by llama.cpp (PCI BDF for PCI GPUs).
    /// Display names are never used as an admission or lease identity.
    pub physical_device_id: Option<String>,
    pub name: String,
    pub description: String,
    pub kind: LlamaCppDeviceKind,
    pub free_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub usable: bool,
    pub supports_gpu_offload: bool,
}

pub fn llama_cpp_device_inventory(native_library_dir: &Path) -> Vec<LlamaCppDeviceInfo> {
    init_llama_backend(native_library_dir);
    let supports_gpu_offload = unsafe { ffi::llama_supports_gpu_offload() };
    let count = unsafe { ffi::ggml_backend_dev_count() };
    let mut devices = Vec::with_capacity(count);
    for index in 0..count {
        let device = unsafe { ffi::ggml_backend_dev_get(index) };
        if device.is_null() {
            continue;
        }
        let name = cstr_to_string(unsafe { ffi::ggml_backend_dev_name(device) })
            .unwrap_or_else(|| format!("device-{index}"));
        let physical_device_id = cstr_to_string(unsafe { ffi::agl_ggml_backend_dev_id(device) });
        let description = cstr_to_string(unsafe { ffi::ggml_backend_dev_description(device) })
            .unwrap_or_else(|| "unknown llama.cpp device".to_string());
        let mut free = 0_usize;
        let mut total = 0_usize;
        unsafe { ffi::ggml_backend_dev_memory(device, &mut free, &mut total) };
        let kind = match unsafe { ffi::ggml_backend_dev_type(device) } {
            ffi::GGML_BACKEND_DEVICE_TYPE_CPU => LlamaCppDeviceKind::Cpu,
            ffi::GGML_BACKEND_DEVICE_TYPE_GPU => LlamaCppDeviceKind::DiscreteGpu,
            ffi::GGML_BACKEND_DEVICE_TYPE_IGPU => LlamaCppDeviceKind::IntegratedGpu,
            ffi::GGML_BACKEND_DEVICE_TYPE_ACCEL => LlamaCppDeviceKind::Accelerator,
            ffi::GGML_BACKEND_DEVICE_TYPE_META => LlamaCppDeviceKind::Metadata,
            _ => LlamaCppDeviceKind::Unknown,
        };
        let gpu = matches!(
            kind,
            LlamaCppDeviceKind::DiscreteGpu | LlamaCppDeviceKind::IntegratedGpu
        );
        devices.push(LlamaCppDeviceInfo {
            physical_device_id,
            name,
            description,
            kind,
            free_memory_bytes: u64::try_from(free).unwrap_or(u64::MAX),
            total_memory_bytes: u64::try_from(total).unwrap_or(u64::MAX),
            usable: kind != LlamaCppDeviceKind::Unknown,
            supports_gpu_offload: gpu && supports_gpu_offload,
        });
    }
    devices
}

/// Convert native backend enumeration into the host-safe runtime contract.
///
/// CPU execution is scoped to the exact worker build. GPU PCI identity and
/// native capability are reported only when llama.cpp exposes them. Untrusted
/// ggml GPU memory counters are represented as `0/0` (unknown); the host must
/// merge fresh sysfs memory and driver evidence before admission. Display
/// names are never substituted for authority identity.
pub fn llama_cpp_inference_device_inventory(
    native_library_dir: &Path,
) -> std::result::Result<Vec<InferenceDeviceInfo>, RuntimeFailure> {
    map_inference_device_inventory(llama_cpp_device_inventory(native_library_dir))
}

fn map_inference_device_inventory(
    devices: Vec<LlamaCppDeviceInfo>,
) -> std::result::Result<Vec<InferenceDeviceInfo>, RuntimeFailure> {
    let mut mapped = Vec::with_capacity(devices.len());
    let mut identities = BTreeSet::new();
    for device in devices {
        let gpu = matches!(
            device.kind,
            LlamaCppDeviceKind::DiscreteGpu | LlamaCppDeviceKind::IntegratedGpu
        );
        if !gpu && device.free_memory_bytes > device.total_memory_bytes {
            return Err(RuntimeFailure::new(
                "llama.cpp device inventory reported impossible memory capacity",
                "",
            ));
        }
        let kind = match device.kind {
            LlamaCppDeviceKind::Cpu => InferenceDeviceKind::Cpu,
            LlamaCppDeviceKind::DiscreteGpu => InferenceDeviceKind::DiscreteGpu,
            LlamaCppDeviceKind::IntegratedGpu => InferenceDeviceKind::IntegratedGpu,
            LlamaCppDeviceKind::Accelerator => InferenceDeviceKind::Accelerator,
            LlamaCppDeviceKind::Metadata => InferenceDeviceKind::Metadata,
            LlamaCppDeviceKind::Unknown => InferenceDeviceKind::Unknown,
        };
        let (
            physical_device_id,
            driver_build_id,
            usable,
            supports_gpu_offload,
            free_memory_bytes,
            total_memory_bytes,
        ) = match device.kind {
            LlamaCppDeviceKind::Cpu => (
                CPU_PHYSICAL_DEVICE_ID.to_string(),
                WORKER_BUILD_ID.to_string(),
                device.usable,
                false,
                device.free_memory_bytes,
                device.total_memory_bytes,
            ),
            LlamaCppDeviceKind::DiscreteGpu | LlamaCppDeviceKind::IntegratedGpu => {
                let physical_device_id = device.physical_device_id.as_deref().ok_or_else(|| {
                    RuntimeFailure::new(
                        "llama.cpp GPU inventory lacks a stable physical-device identity",
                        "",
                    )
                })?;
                let pci_bdf = canonical_pci_bdf(physical_device_id)?;
                (
                    format!("pci:{pci_bdf}"),
                    // This exact marker proves the worker build, not the GPU
                    // driver. The host replaces it with matching sysfs driver
                    // evidence before using this native capability.
                    WORKER_BUILD_ID.to_string(),
                    device.usable,
                    device.supports_gpu_offload,
                    // ggml may report process-accounting values here, and has
                    // produced free > total in practice. The wire contract
                    // uses 0/0 as unknown until the host merges kernel sysfs.
                    0,
                    0,
                )
            }
            LlamaCppDeviceKind::Accelerator
            | LlamaCppDeviceKind::Metadata
            | LlamaCppDeviceKind::Unknown => {
                let physical_device_id = device.physical_device_id.ok_or_else(|| {
                    RuntimeFailure::new(
                        "llama.cpp non-CPU inventory lacks a stable physical-device identity",
                        "",
                    )
                })?;
                if physical_device_id.is_empty() || physical_device_id.len() > 256 {
                    return Err(RuntimeFailure::new(
                        "llama.cpp physical-device identity is invalid",
                        "",
                    ));
                }
                (
                    physical_device_id,
                    WORKER_BUILD_ID.to_string(),
                    false,
                    false,
                    device.free_memory_bytes,
                    device.total_memory_bytes,
                )
            }
        };
        if !identities.insert(physical_device_id.clone()) {
            return Err(RuntimeFailure::new(
                "llama.cpp device inventory contains duplicate physical identities",
                "",
            ));
        }
        mapped.push(InferenceDeviceInfo {
            physical_device_id,
            driver_build_id,
            backend_name: device.name,
            description: device.description,
            kind,
            free_memory_bytes,
            total_memory_bytes,
            usable,
            supports_gpu_offload,
        });
    }
    Ok(mapped)
}

fn canonical_pci_bdf(value: &str) -> std::result::Result<String, RuntimeFailure> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && bytes[..4].iter().all(u8::is_ascii_hexdigit)
        && bytes[5..7].iter().all(u8::is_ascii_hexdigit)
        && bytes[8..10].iter().all(u8::is_ascii_hexdigit)
        && bytes[11].is_ascii_hexdigit();
    if !valid {
        return Err(RuntimeFailure::new(
            "llama.cpp GPU physical identity is not canonical PCI BDF syntax",
            "",
        ));
    }
    let device = u8::from_str_radix(&value[8..10], 16).unwrap_or(u8::MAX);
    let function = u8::from_str_radix(&value[11..12], 16).unwrap_or(u8::MAX);
    if device > 0x1f || function > 7 {
        return Err(RuntimeFailure::new(
            "llama.cpp GPU physical identity is outside PCI BDF bounds",
            "",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

impl ModelRuntime for LlamaCppModelRuntime {
    type Model = LlamaCppModel;
    type Context = LlamaCppContextSlot;

    fn device_inventory(
        &mut self,
    ) -> std::result::Result<Vec<InferenceDeviceInfo>, RuntimeFailure> {
        llama_cpp_inference_device_inventory(&self.native_library_dir)
    }

    fn load_model(
        &mut self,
        job: &InferenceJob,
    ) -> std::result::Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
        let config = job.config();
        require_known_gpu_shape(config)?;
        let mut operation = capture_operation(|log| {
            init_llama_backend(&self.native_library_dir);
            let supports_gpu_offload = unsafe { ffi::llama_supports_gpu_offload() };
            log.push_str(&runtime_log_header(
                config,
                supports_gpu_offload,
                &self.native_library_dir,
            ));
            log.push_str("llama_cpp_operation = load_model\n");
            if let Some(message) =
                gpu_offload_unavailable_message(config.runtime.gpu_layers, supports_gpu_offload)
            {
                anyhow::bail!(message);
            }
            LlamaCppModel::load(config, log)
        })?;
        let selected_device = resolve_selected_device(
            config.runtime.device.as_deref(),
            &operation.log,
            operation.value.metadata().selected_device.as_deref(),
        );
        operation.value.record_selected_device(selected_device);
        Ok(operation)
    }

    fn create_context(
        &mut self,
        model: &mut Self::Model,
        job: &InferenceJob,
    ) -> std::result::Result<RuntimeOperation<Self::Context>, RuntimeFailure> {
        capture_operation(|log| {
            log.push_str("llama_cpp_operation = create_context\n");
            ensure!(
                model.matches_config(job.config()),
                "loaded llama.cpp model resources do not match the inference job"
            );
            LlamaCppContextSlot::new(model, job.config(), log)
        })
    }

    fn generate(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
        job: &InferenceJob,
    ) -> std::result::Result<RuntimeOperation<ModelGeneration>, RuntimeFailure> {
        let has_media = resolved_image_count(job.resolved_content()) > 0;
        let result = capture_operation(|log| {
            let supports_gpu_offload = unsafe { ffi::llama_supports_gpu_offload() };
            log.push_str(&runtime_log_header(
                job.config(),
                supports_gpu_offload,
                &self.native_library_dir,
            ));
            log.push_str("llama_cpp_operation = generate\n");
            ensure!(
                model.matches_config(job.config()),
                "loaded llama.cpp model resources do not match the inference job"
            );
            if has_media {
                log.push_str("llama_cpp_context_reset_reason = multimodal_request_rebuild\n");
                context.reset_cache(model, job.config(), log)?;
            } else if !context.matches_config(job.config()) {
                log.push_str("llama_cpp_context_reset_reason = context_config_changed\n");
                context.reset_cache(model, job.config(), log)?;
            } else if let Some(reason) =
                context.rendered_append_error(model, &job.request().rendered)
            {
                log.push_str("llama_cpp_context_reset_reason = rendered_history_not_appendable\n");
                log.push_str("llama_cpp_context_reset_detail = ");
                log.push_str(&reason);
                log.push('\n');
                context.reset_cache(model, job.config(), log)?;
            }
            let control =
                LlamaCppGenerationControl::cancellable_until(job.cancellation(), job.deadline());
            let output = if has_media {
                let marker = model
                    .vision_marker()
                    .ok_or_else(|| anyhow::anyhow!("llama.cpp model has no vision marker"))?;
                let prepared = prepare_vision_request(
                    &job.request().rendered,
                    job.resolved_content()
                        .ok_or_else(|| anyhow::anyhow!("resolved multimodal content is missing"))?,
                    marker,
                )?;
                context.generate_vision(
                    model,
                    LlamaCppGenerationRequest::new(
                        &prepared.rendered,
                        job.max_output_tokens(),
                        &job.request().attempt_id,
                        job.output_sink(),
                    ),
                    &prepared.images,
                    &control,
                    log,
                )?
            } else {
                context.generate(
                    model,
                    LlamaCppGenerationRequest::new(
                        &job.request().rendered,
                        job.max_output_tokens(),
                        &job.request().attempt_id,
                        job.output_sink(),
                    ),
                    &control,
                    log,
                )?
            };
            Ok(ModelGeneration {
                content: output.content,
                finish_reason: output.finish_reason,
                selected_device: model.metadata().selected_device.clone(),
                input_tokens: output.input_tokens,
                output_tokens: output.output_tokens,
            })
        });
        if has_media {
            result.map_err(RuntimeFailure::into_multimodal_encode)
        } else {
            result
        }
    }

    fn clear_context(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
        capture_operation(|log| {
            log.push_str("llama_cpp_operation = clear_context\n");
            context.clear_cache(model, log)
        })
    }

    fn release_context(
        &mut self,
        _model: &mut Self::Model,
        _context: &mut Self::Context,
    ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
        Ok(RuntimeOperation::new(
            (),
            "llama_cpp_operation = release_context\n",
        ))
    }

    fn release_model(
        &mut self,
        _model: &mut Self::Model,
    ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
        Ok(RuntimeOperation::new(
            (),
            "llama_cpp_operation = release_model\n",
        ))
    }
}

impl WorkerServiceRuntime for LlamaCppModelRuntime {
    fn allocation_receipt(
        &self,
        _model: &Self::Model,
        context: &Self::Context,
        job: &InferenceJob,
    ) -> std::result::Result<AllocationReceipt, RuntimeFailure> {
        let Some(_profile) = require_known_gpu_shape(job.config())? else {
            return AllocationReceipt::new(0, 0, 0, None)
                .map_err(|error| RuntimeFailure::new(error.to_string(), ""));
        };
        let selector =
            job.config().runtime.device.as_deref().ok_or_else(|| {
                RuntimeFailure::new("known GPU profile has no device selector", "")
            })?;
        let device = llama_cpp_device_inventory(&self.native_library_dir)
            .into_iter()
            .find(|device| device.name == selector)
            .ok_or_else(|| {
                RuntimeFailure::new(
                    "known GPU profile selector disappeared from native inventory",
                    "",
                )
            })?;
        if !device.usable || !device.supports_gpu_offload {
            return Err(RuntimeFailure::new(
                "known GPU profile device is not usable for native offload",
                "",
            ));
        }
        let physical = device.physical_device_id.as_deref().ok_or_else(|| {
            RuntimeFailure::new("known GPU profile device lacks a physical PCI identity", "")
        })?;
        let selector_c = CString::new(selector).map_err(|_| {
            RuntimeFailure::new("known GPU profile device selector contains NUL", "")
        })?;
        let backend_device = unsafe { ffi::ggml_backend_dev_by_name(selector_c.as_ptr()) };
        if backend_device.is_null() {
            return Err(RuntimeFailure::new(
                "known GPU profile selector disappeared before allocation receipt",
                "",
            ));
        }
        let receipt = measured_device_allocation(context, backend_device)
            .map_err(|error| RuntimeFailure::new(error.to_string(), ""))?;
        AllocationReceipt::new(
            receipt.model_bytes,
            receipt.context_bytes,
            receipt.compute_bytes,
            Some(format!("pci:{}", canonical_pci_bdf(physical)?)),
        )
        .map_err(|error| RuntimeFailure::new(error.to_string(), ""))
    }

    fn failure_code(&self, failure: &RuntimeFailure) -> WorkerFailureCode {
        if failure.is_backend_lost() {
            WorkerFailureCode::DeviceLost
        } else {
            WorkerFailureCode::RuntimeFailure
        }
    }
}

fn measured_device_allocation(
    context: &LlamaCppContextSlot,
    device: ffi::ggml_backend_dev_t,
) -> Result<ffi::agl_llama_device_memory_breakdown> {
    let mut total = ffi::agl_llama_device_memory_breakdown::default();
    let mut found = false;
    for context_ptr in
        std::iter::once(context.main_context_ptr()).chain(context.draft_context_ptr())
    {
        let mut current = ffi::agl_llama_device_memory_breakdown::default();
        let status = unsafe {
            ffi::agl_llama_context_device_memory_breakdown(context_ptr, device, &mut current)
        };
        ensure!(
            status == 0,
            "llama.cpp device allocation breakdown failed with status {status}"
        );
        ensure!(
            current.found <= 1,
            "llama.cpp device allocation breakdown returned an invalid presence marker"
        );
        found |= current.found == 1;
        total.model_bytes = total
            .model_bytes
            .checked_add(current.model_bytes)
            .ok_or_else(|| anyhow::anyhow!("llama.cpp model allocation receipt overflow"))?;
        total.context_bytes = total
            .context_bytes
            .checked_add(current.context_bytes)
            .ok_or_else(|| anyhow::anyhow!("llama.cpp context allocation receipt overflow"))?;
        total.compute_bytes = total
            .compute_bytes
            .checked_add(current.compute_bytes)
            .ok_or_else(|| anyhow::anyhow!("llama.cpp compute allocation receipt overflow"))?;
    }
    ensure!(
        found,
        "llama.cpp reported no allocated buffers on the admitted GPU"
    );
    total.found = 1;
    Ok(total)
}

fn requests_gpu_allocation(config: &ResolvedInferenceConfig) -> bool {
    config.runtime.gpu_layers > 0
        || (config.runtime.mtp.enabled
            && config
                .runtime
                .mtp
                .gpu_layers
                .unwrap_or(config.runtime.gpu_layers)
                > 0)
}

fn require_known_gpu_shape(
    config: &ResolvedInferenceConfig,
) -> std::result::Result<Option<agl_inference::gpu_profile::KnownGpuProfile>, RuntimeFailure> {
    match agl_inference::gpu_profile::known_gpu_profile_shape(config) {
        Ok(profile) => Ok(profile),
        Err(error) if requests_gpu_allocation(config) => Err(RuntimeFailure::resource_admission(
            error.code(),
            error.to_string(),
            "",
        )),
        Err(error) => Err(RuntimeFailure::new(error.to_string(), "")),
    }
}

struct PreparedVisionRequest<'a> {
    rendered: RenderedModelRequest,
    images: Vec<&'a [u8]>,
}

fn resolved_image_count(content: Option<&ResolvedModelContent>) -> usize {
    content
        .into_iter()
        .flat_map(ResolvedModelContent::messages)
        .flat_map(|message| message.parts())
        .filter(|part| matches!(part, ResolvedContentPart::Image { .. }))
        .count()
}

fn prepare_vision_request<'a>(
    rendered: &RenderedModelRequest,
    resolved: &'a ResolvedModelContent,
    marker: &str,
) -> Result<PreparedVisionRequest<'a>> {
    ensure!(!marker.is_empty(), "llama.cpp vision marker is empty");
    ensure!(
        rendered.messages.len() == resolved.messages().len(),
        "resolved content does not match the rendered message count"
    );
    let mut prepared = rendered.clone();
    let mut images = Vec::new();
    for (message, resolved_message) in prepared.messages.iter_mut().zip(resolved.messages()) {
        message.content = render_vision_content(resolved_message.parts(), marker, &mut images)?;
    }
    ensure!(
        !images.is_empty(),
        "resolved multimodal content has no images"
    );
    Ok(PreparedVisionRequest {
        rendered: prepared,
        images,
    })
}

fn render_vision_content<'a>(
    parts: &'a [ResolvedContentPart],
    marker: &str,
    images: &mut Vec<&'a [u8]>,
) -> Result<Option<Content>> {
    let mut text = String::new();
    for part in parts {
        match part {
            ResolvedContentPart::Text { text: part } => {
                ensure!(
                    !part.contains(marker),
                    "text content contains the reserved llama.cpp vision marker"
                );
                text.push_str(part);
            }
            ResolvedContentPart::Image { bytes, .. } => {
                text.push_str(marker);
                images.push(bytes.as_slice());
            }
        }
    }
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Content::text(text)?))
    }
}

fn capture_operation<T>(
    operation: impl FnOnce(&mut String) -> Result<T>,
) -> std::result::Result<RuntimeOperation<T>, RuntimeFailure> {
    let capture = NativeLogCapture::begin()
        .map_err(|error| RuntimeFailure::new(format!("{error:#}"), String::new()))?;
    let mut log = String::new();
    let result = operation(&mut log);
    let log = finish_runtime_log(log, capture.finish());
    match result {
        Ok(value) => Ok(RuntimeOperation::new(value, log)),
        Err(error) => Err(RuntimeFailure::new(format!("{error:#}"), log)),
    }
}

pub(crate) fn init_llama_backend(native_library_dir: &Path) {
    LLAMA_BACKEND.get_or_init(|| {
        let lib_dir = CString::new(native_library_dir.as_os_str().as_encoded_bytes())
            .expect("valid llama.cpp native bundle directory");
        unsafe {
            ffi::llama_log_set(Some(llama_log_callback), ptr::null_mut());
            ffi::mtmd_log_set(Some(llama_log_callback), ptr::null_mut());
            ffi::ggml_backend_load_all_from_path(lib_dir.as_ptr());
            ffi::llama_backend_init();
        }
    });
}

unsafe extern "C" fn llama_log_callback(
    _level: c_int,
    text: *const c_char,
    _user_data: *mut c_void,
) {
    if let Some(text) = cstr_to_string(text)
        && let Ok(mut state) = LLAMA_LOGS.lock()
        && let Some(logs) = state.active.as_mut()
    {
        logs.push(&text);
    }
}

impl NativeLogBuffer {
    fn push(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let payload_cap = MAX_NATIVE_LOG_BYTES.saturating_sub(NATIVE_LOG_TRUNCATION_MARKER.len());
        let remaining = payload_cap.saturating_sub(self.content.len());
        if text.len() <= remaining {
            self.content.push_str(text);
            return;
        }

        let mut keep = remaining.min(text.len());
        while keep > 0 && !text.is_char_boundary(keep) {
            keep -= 1;
        }
        self.content.push_str(&text[..keep]);
        self.truncated = true;
    }

    fn finish(mut self) -> String {
        if self.truncated {
            self.content.push_str(NATIVE_LOG_TRUNCATION_MARKER);
        }
        self.content
    }
}

impl NativeLogCapture {
    pub(crate) fn begin() -> Result<Self> {
        let mut state = LLAMA_LOGS
            .lock()
            .map_err(|_| anyhow::anyhow!("llama.cpp native log capture lock is poisoned"))?;
        ensure!(
            state.active.is_none(),
            "llama.cpp native operation already has an active log capture"
        );
        state.active = Some(NativeLogBuffer::default());
        Ok(Self { active: true })
    }

    pub(crate) fn finish(mut self) -> String {
        self.active = false;
        LLAMA_LOGS
            .lock()
            .ok()
            .and_then(|mut state| state.active.take())
            .map(NativeLogBuffer::finish)
            .unwrap_or_default()
    }
}

impl Drop for NativeLogCapture {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut state) = LLAMA_LOGS.lock() {
            state.active = None;
        }
    }
}

fn finish_runtime_log(mut log: String, native_logs: String) -> String {
    if !native_logs.is_empty() {
        log.push_str("llama_cpp_log:\n");
        log.push_str(&native_logs);
        if !native_logs.ends_with('\n') {
            log.push('\n');
        }
    }
    log
}

fn gpu_offload_unavailable_message(gpu_layers: u32, supports_gpu_offload: bool) -> Option<String> {
    if gpu_layers == 0 || supports_gpu_offload {
        return None;
    }

    Some(format!(
        "llama.cpp GPU offload requested with gpu_layers={gpu_layers}, but no GPU backend is available. Set [runtime].gpu_layers = 0 for CPU-only runs or make a llama.cpp GPU backend available to this process."
    ))
}

fn resolve_selected_device(
    configured_device: Option<&str>,
    current_native_logs: &str,
    prior_selected_device: Option<&str>,
) -> Option<String> {
    configured_device
        .map(str::to_string)
        .or_else(|| selected_device_from_llama_logs(current_native_logs))
        .or_else(|| prior_selected_device.map(str::to_string))
}

fn selected_device_from_llama_logs(log: &str) -> Option<String> {
    const PREFIX: &str = "llama_prepare_model_devices: using device ";
    for line in log.lines() {
        let Some(rest) = line.strip_prefix(PREFIX) else {
            continue;
        };
        let device = rest
            .split_once(" (")
            .map(|(name, _)| name)
            .unwrap_or(rest)
            .trim();
        if !device.is_empty() {
            return Some(device.to_string());
        }
    }
    None
}

fn runtime_log_header(
    config: &ResolvedInferenceConfig,
    supports_gpu_offload: bool,
    native_library_dir: &Path,
) -> String {
    let mut log = String::new();
    log.push_str("backend = llama_cpp\n");
    log.push_str("library_dir = ");
    log.push_str(&native_library_dir.to_string_lossy());
    log.push('\n');
    log.push_str("gpu_layers_requested = ");
    log.push_str(&config.runtime.gpu_layers.to_string());
    log.push('\n');
    log.push_str("supports_gpu_offload = ");
    log.push_str(if supports_gpu_offload {
        "true"
    } else {
        "false"
    });
    log.push('\n');
    append_mtp_config_log(&mut log, &config.runtime.mtp);
    log.push_str("devices:\n");
    log.push_str(&available_devices());
    if let Some(system_info) = cstr_to_string(unsafe { ffi::llama_print_system_info() }) {
        log.push_str("system_info = ");
        log.push_str(&system_info);
        log.push('\n');
    }
    log
}

fn append_mtp_config_log(log: &mut String, mtp: &MtpRuntimeConfig) {
    log.push_str("mtp_enabled = ");
    log.push_str(if mtp.enabled { "true" } else { "false" });
    log.push('\n');
    if let Some(path) = &mtp.draft_model {
        log.push_str("mtp_draft_model = ");
        log.push_str(&path.display().to_string());
        log.push('\n');
    }
    if mtp.draft_tokens > 0 {
        log.push_str("mtp_draft_tokens = ");
        log.push_str(&mtp.draft_tokens.to_string());
        log.push('\n');
    }
    if mtp.enabled || !mtp.p_min.is_zero() {
        log.push_str("mtp_p_min = ");
        log.push_str(&mtp.p_min.as_f32().to_string());
        log.push('\n');
    }
    if let Some(gpu_layers) = mtp.gpu_layers {
        log.push_str("mtp_gpu_layers = ");
        log.push_str(&gpu_layers.to_string());
        log.push('\n');
    }
    if let Some(cache_type) = mtp.cache_type_k {
        log.push_str("mtp_cache_type_k = ");
        log.push_str(kv_cache_type_name(cache_type));
        log.push('\n');
    }
    if let Some(cache_type) = mtp.cache_type_v {
        log.push_str("mtp_cache_type_v = ");
        log.push_str(kv_cache_type_name(cache_type));
        log.push('\n');
    }
}

fn kv_cache_type_name(cache_type: KvCacheType) -> &'static str {
    match cache_type {
        KvCacheType::F32 => "f32",
        KvCacheType::F16 => "f16",
        KvCacheType::Bf16 => "bf16",
        KvCacheType::Q8_0 => "q8_0",
        KvCacheType::Q4_0 => "q4_0",
        KvCacheType::Q4_1 => "q4_1",
        KvCacheType::Iq4Nl => "iq4_nl",
        KvCacheType::Q5_0 => "q5_0",
        KvCacheType::Q5_1 => "q5_1",
    }
}

fn available_devices() -> String {
    let mut devices = String::new();
    let count = unsafe { ffi::ggml_backend_dev_count() };
    for index in 0..count {
        let device = unsafe { ffi::ggml_backend_dev_get(index) };
        let name = cstr_to_string(unsafe { ffi::ggml_backend_dev_name(device) })
            .unwrap_or_else(|| "<unknown>".to_string());
        let description = cstr_to_string(unsafe { ffi::ggml_backend_dev_description(device) })
            .unwrap_or_else(|| "<unknown>".to_string());
        let mut free = 0;
        let mut total = 0;
        unsafe { ffi::ggml_backend_dev_memory(device, &mut free, &mut total) };
        devices.push_str("- ");
        devices.push_str(&name);
        devices.push_str(": ");
        devices.push_str(&description);
        if total > 0 {
            devices.push_str(" (");
            devices.push_str(&(free / 1024 / 1024).to_string());
            devices.push_str(" MiB free / ");
            devices.push_str(&(total / 1024 / 1024).to_string());
            devices.push_str(" MiB total)");
        }
        devices.push('\n');
    }
    devices
}

fn cstr_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use agl_content::{
        ArtifactId, ArtifactRef, ArtifactSensitivity, ArtifactSource, ArtifactSourceKind,
        BlobDigest, ImageDimensions, MediaType,
    };

    use super::*;

    static NATIVE_LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn native_log_test_guard() -> std::sync::MutexGuard<'static, ()> {
        NATIVE_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn image_part(bytes: Vec<u8>) -> ResolvedContentPart {
        let artifact = ArtifactRef::new(
            ArtifactId::generate(),
            BlobDigest::from_bytes(&bytes),
            MediaType::ImagePng,
            u64::try_from(bytes.len()).unwrap(),
            Some(ImageDimensions::new(1, 1).unwrap()),
            ArtifactSensitivity::Sensitive,
            ArtifactSource {
                kind: ArtifactSourceKind::ScreenCapture,
                provider: Some("test".to_string()),
            },
        )
        .unwrap();
        ResolvedContentPart::Image { artifact, bytes }
    }

    #[test]
    fn vision_content_preserves_interleaved_text_and_image_order() {
        let parts = vec![
            ResolvedContentPart::Text {
                text: "before ".to_string(),
            },
            image_part(vec![1]),
            ResolvedContentPart::Text {
                text: " middle ".to_string(),
            },
            image_part(vec![2, 3]),
            ResolvedContentPart::Text {
                text: " after".to_string(),
            },
        ];
        let mut images = Vec::new();

        let content = render_vision_content(&parts, "<image>", &mut images)
            .unwrap()
            .unwrap();

        assert_eq!(
            content.text_only().as_deref(),
            Some("before <image> middle <image> after")
        );
        assert_eq!(images, vec![&[1][..], &[2, 3][..]]);
    }

    #[test]
    fn vision_content_rejects_literal_reserved_marker() {
        let parts = [ResolvedContentPart::Text {
            text: "literal <image> marker".to_string(),
        }];
        let mut images = Vec::new();

        let error = render_vision_content(&parts, "<image>", &mut images).unwrap_err();

        assert!(error.to_string().contains("reserved"));
        assert!(images.is_empty());
    }

    fn assert_send<T: Send>() {}

    #[test]
    fn model_runtime_can_move_to_worker_before_loading_native_resources() {
        assert_send::<LlamaCppModelRuntime>();
    }

    #[test]
    fn allocation_breakdown_bridge_rejects_null_native_handles() {
        let mut breakdown = ffi::agl_llama_device_memory_breakdown::default();
        let status = unsafe {
            ffi::agl_llama_context_device_memory_breakdown(
                std::ptr::null(),
                std::ptr::null_mut(),
                &mut breakdown,
            )
        };
        assert_eq!(status, -1);
        assert_eq!(breakdown, ffi::agl_llama_device_memory_breakdown::default());
    }

    #[test]
    fn host_safe_inventory_uses_only_proven_authority_evidence() {
        let mapped = map_inference_device_inventory(vec![
            LlamaCppDeviceInfo {
                physical_device_id: None,
                name: "CPU".to_string(),
                description: "host CPU".to_string(),
                kind: LlamaCppDeviceKind::Cpu,
                free_memory_bytes: 100,
                total_memory_bytes: 200,
                usable: true,
                supports_gpu_offload: false,
            },
            LlamaCppDeviceInfo {
                physical_device_id: Some("0000:0A:1F.7".to_string()),
                name: "Vulkan0".to_string(),
                description: "display-only GPU description".to_string(),
                kind: LlamaCppDeviceKind::DiscreteGpu,
                free_memory_bytes: 500,
                total_memory_bytes: 400,
                usable: true,
                supports_gpu_offload: true,
            },
        ])
        .expect("map proven native identities");

        assert_eq!(mapped[0].physical_device_id, CPU_PHYSICAL_DEVICE_ID);
        assert_eq!(mapped[0].driver_build_id, WORKER_BUILD_ID);
        assert!(mapped[0].usable);
        assert!(!mapped[0].supports_gpu_offload);
        assert_eq!(mapped[1].physical_device_id, "pci:0000:0a:1f.7");
        assert_eq!(mapped[1].driver_build_id, WORKER_BUILD_ID);
        assert_eq!(mapped[1].backend_name, "Vulkan0");
        assert!(mapped[1].usable);
        assert!(mapped[1].supports_gpu_offload);
        assert_eq!(mapped[1].free_memory_bytes, 0);
        assert_eq!(mapped[1].total_memory_bytes, 0);
    }

    #[test]
    fn gpu_inventory_never_uses_backend_or_display_names_as_identity() {
        let error = map_inference_device_inventory(vec![LlamaCppDeviceInfo {
            physical_device_id: None,
            name: "Vulkan0".to_string(),
            description: "tempting display name".to_string(),
            kind: LlamaCppDeviceKind::DiscreteGpu,
            free_memory_bytes: 300,
            total_memory_bytes: 400,
            usable: true,
            supports_gpu_offload: true,
        }])
        .expect_err("GPU without physical identity must fail closed");

        assert!(error.message().contains("physical-device identity"));
    }

    #[test]
    fn device_loss_classification_uses_typed_runtime_kind_only() {
        let runtime = LlamaCppModelRuntime::new();
        let ordinary = RuntimeFailure::new("identical message", "VK_ERROR_DEVICE_LOST text");
        let lost = RuntimeFailure::backend_lost("identical message", "unstructured text ignored");

        assert_eq!(
            runtime.failure_code(&ordinary),
            WorkerFailureCode::RuntimeFailure
        );
        assert_eq!(runtime.failure_code(&lost), WorkerFailureCode::DeviceLost);
    }

    #[test]
    fn extracts_auto_selected_llama_device() {
        let log = "\
llama_model_loader: metadata
llama_prepare_model_devices: using device Vulkan0 (AMD Radeon RX 7900 XTX) - 22938 MiB free
load_tensors: offloaded 34/34 layers to GPU
";

        assert_eq!(
            selected_device_from_llama_logs(log).as_deref(),
            Some("Vulkan0")
        );
    }

    #[test]
    fn selected_device_prefers_configured_value() {
        let log = "llama_prepare_model_devices: using device Vulkan0 (auto)\n";

        assert_eq!(
            resolve_selected_device(Some("Vulkan1"), log, None).as_deref(),
            Some("Vulkan1")
        );
    }

    #[test]
    fn selected_device_can_use_prior_model_metadata() {
        assert_eq!(
            resolve_selected_device(None, "", Some("Vulkan0")).as_deref(),
            Some("Vulkan0")
        );
    }

    #[test]
    fn selected_device_is_none_when_unavailable() {
        assert_eq!(
            resolve_selected_device(None, "no selected device", None),
            None
        );
    }

    #[test]
    fn gpu_offload_unavailable_only_when_requested_and_unsupported() {
        assert!(gpu_offload_unavailable_message(0, false).is_none());
        assert!(gpu_offload_unavailable_message(99, true).is_none());

        let message = gpu_offload_unavailable_message(99, false).unwrap();

        assert!(message.contains("gpu_layers=99"));
        assert!(message.contains("gpu_layers = 0"));
    }

    #[test]
    fn sequential_native_log_captures_do_not_cross_boundaries() {
        let _guard = native_log_test_guard();
        let first = NativeLogCapture::begin().unwrap();
        let first_message = CString::new("first operation\n").unwrap();
        unsafe { llama_log_callback(0, first_message.as_ptr(), ptr::null_mut()) };
        let first_log = first.finish();

        let outside_message = CString::new("outside capture\n").unwrap();
        unsafe { llama_log_callback(0, outside_message.as_ptr(), ptr::null_mut()) };

        let second = NativeLogCapture::begin().unwrap();
        let second_message = CString::new("second operation\n").unwrap();
        unsafe { llama_log_callback(0, second_message.as_ptr(), ptr::null_mut()) };
        let second_log = second.finish();

        assert_eq!(first_log, "first operation\n");
        assert_eq!(second_log, "second operation\n");
    }

    #[test]
    fn native_log_capture_rejects_overlapping_operations() {
        let _guard = native_log_test_guard();
        let capture = NativeLogCapture::begin().unwrap();

        let error = NativeLogCapture::begin().err().unwrap();

        assert!(
            error
                .to_string()
                .contains("already has an active log capture")
        );
        drop(capture);
        assert!(NativeLogCapture::begin().is_ok());
    }

    #[test]
    fn dropped_native_log_capture_discards_partial_operation_log() {
        let _guard = native_log_test_guard();
        let abandoned = NativeLogCapture::begin().unwrap();
        let abandoned_message = CString::new("abandoned operation\n").unwrap();
        unsafe { llama_log_callback(0, abandoned_message.as_ptr(), ptr::null_mut()) };
        drop(abandoned);

        let next = NativeLogCapture::begin().unwrap();
        let next_message = CString::new("next operation\n").unwrap();
        unsafe { llama_log_callback(0, next_message.as_ptr(), ptr::null_mut()) };

        assert_eq!(next.finish(), "next operation\n");
    }

    #[test]
    fn runtime_operation_keeps_only_its_scoped_native_log() {
        let _guard = native_log_test_guard();
        let operation = capture_operation(|log| {
            log.push_str("logical operation\n");
            let native = CString::new("native operation\n").unwrap();
            unsafe { llama_log_callback(0, native.as_ptr(), ptr::null_mut()) };
            Ok(7_u8)
        })
        .unwrap();

        assert_eq!(operation.value, 7);
        assert!(operation.log.contains("logical operation\n"));
        assert!(operation.log.contains("llama_cpp_log:\nnative operation\n"));
    }

    #[test]
    fn native_log_capture_is_bounded_and_marks_truncation() {
        let _guard = native_log_test_guard();
        let capture = NativeLogCapture::begin().unwrap();
        let oversized = "ж".repeat(MAX_NATIVE_LOG_BYTES);
        let message = CString::new(oversized).unwrap();
        unsafe { llama_log_callback(0, message.as_ptr(), ptr::null_mut()) };

        let captured = capture.finish();
        assert!(captured.len() <= MAX_NATIVE_LOG_BYTES);
        assert!(captured.ends_with(NATIVE_LOG_TRUNCATION_MARKER));
        assert!(captured.is_char_boundary(captured.len()));
    }

    #[test]
    fn runtime_failure_preserves_the_failed_operation_log() {
        let _guard = native_log_test_guard();
        let failure = capture_operation::<()>(|log| {
            log.push_str("before failure\n");
            anyhow::bail!("native failure")
        })
        .unwrap_err();

        assert_eq!(failure.message(), "native failure");
        assert_eq!(failure.log(), "before failure\n");
    }
}
