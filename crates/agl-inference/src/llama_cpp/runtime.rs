use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use crate::model_manager::{
    InferenceJob, ModelGeneration, ModelKey, ModelRuntime, RuntimeFailure, RuntimeOperation,
};
use agl_config::{KvCacheType, LocalInferenceConfig, MtpRuntimeConfig};
use anyhow::{Result, ensure};

use super::context_slot::LlamaCppContextSlot;
use super::ffi;
use super::generation::LlamaCppGenerationControl;
use super::model::LlamaCppModel;

static LLAMA_BACKEND: OnceLock<()> = OnceLock::new();
static LLAMA_LOGS: Mutex<NativeLogState> = Mutex::new(NativeLogState { active: None });

struct NativeLogState {
    active: Option<String>,
}

pub(crate) struct NativeLogCapture {
    active: bool,
}

#[derive(Default)]
pub struct LlamaCppModelRuntime;

impl LlamaCppModelRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl ModelRuntime for LlamaCppModelRuntime {
    type Model = LlamaCppModel;
    type Context = LlamaCppContextSlot;

    fn load_model(
        &mut self,
        _key: &ModelKey,
        config: &LocalInferenceConfig,
    ) -> std::result::Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
        let mut operation = capture_operation(|log| {
            init_llama_backend();
            let supports_gpu_offload = unsafe { ffi::llama_supports_gpu_offload() };
            log.push_str(&runtime_log_header(config, supports_gpu_offload));
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
        capture_operation(|log| {
            let supports_gpu_offload = unsafe { ffi::llama_supports_gpu_offload() };
            log.push_str(&runtime_log_header(job.config(), supports_gpu_offload));
            log.push_str("llama_cpp_operation = generate\n");
            ensure!(
                model.matches_config(job.config()),
                "loaded llama.cpp model resources do not match the inference job"
            );
            if !context.matches_config(job.config()) {
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
            let control = LlamaCppGenerationControl::cancellable_until(
                job.cancellation().atomic_flag(),
                job.deadline(),
            );
            let output = context.generate(
                model,
                &job.request().rendered,
                job.max_output_tokens(),
                &control,
                log,
            )?;
            Ok(ModelGeneration {
                content: output.content,
                finish_reason: output.finish_reason,
                selected_device: model.metadata().selected_device.clone(),
                input_tokens: output.input_tokens,
                output_tokens: output.output_tokens,
            })
        })
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

pub(crate) fn init_llama_backend() {
    LLAMA_BACKEND.get_or_init(|| {
        let lib_dir =
            CString::new(env!("AGL_LLAMA_CPP_LIBRARY_DIR")).expect("valid llama.cpp lib dir");
        unsafe {
            ffi::llama_log_set(Some(llama_log_callback), ptr::null_mut());
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
        logs.push_str(&text);
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
        state.active = Some(String::new());
        Ok(Self { active: true })
    }

    pub(crate) fn finish(mut self) -> String {
        self.active = false;
        LLAMA_LOGS
            .lock()
            .ok()
            .and_then(|mut state| state.active.take())
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

fn runtime_log_header(config: &LocalInferenceConfig, supports_gpu_offload: bool) -> String {
    let mut log = String::new();
    log.push_str("backend = llama_cpp\n");
    log.push_str("library_dir = ");
    log.push_str(env!("AGL_LLAMA_CPP_LIBRARY_DIR"));
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
    use super::*;

    fn assert_send<T: Send>() {}

    #[test]
    fn model_runtime_can_move_to_worker_before_loading_native_resources() {
        assert_send::<LlamaCppModelRuntime>();
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
    fn runtime_failure_preserves_the_failed_operation_log() {
        let failure = capture_operation::<()>(|log| {
            log.push_str("before failure\n");
            anyhow::bail!("native failure")
        })
        .unwrap_err();

        assert_eq!(failure.message(), "native failure");
        assert_eq!(failure.log(), "before failure\n");
    }
}
