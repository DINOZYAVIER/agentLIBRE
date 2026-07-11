use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;

use agl_config::{LocalInferenceConfig, RuntimeSwitch};
use anyhow::{Context, Result, bail, ensure};

use super::ffi;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlamaCppModelKey {
    model: PathBuf,
    multimodal_projector: Option<PathBuf>,
    gpu_layers: u32,
    device: Option<String>,
    mmap: Option<bool>,
    draft_model: Option<PathBuf>,
    draft_gpu_layers: Option<u32>,
}

impl LlamaCppModelKey {
    pub(crate) fn from_config(config: &LocalInferenceConfig) -> Self {
        let mtp_enabled = config.runtime.mtp.enabled;
        Self {
            model: config.backend.model.clone(),
            multimodal_projector: config.backend.multimodal_projector.clone(),
            gpu_layers: config.runtime.gpu_layers,
            device: config.runtime.device.clone(),
            mmap: config.runtime.mmap,
            draft_model: mtp_enabled
                .then(|| config.runtime.mtp.draft_model.clone())
                .flatten(),
            draft_gpu_layers: mtp_enabled.then_some(
                config
                    .runtime
                    .mtp
                    .gpu_layers
                    .unwrap_or(config.runtime.gpu_layers),
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlamaCppModelLoadMetadata {
    pub(crate) description: String,
    pub(crate) draft_description: Option<String>,
    pub(crate) selected_device: Option<String>,
}

/// Worker-thread-owned llama.cpp weights and load-time resources.
///
/// Contexts borrow these resources only while the worker is executing an
/// operation. The `Rc` marker makes that thread affinity explicit without an
/// unsafe `Send` or `Sync` implementation.
pub struct LlamaCppModel {
    key: LlamaCppModelKey,
    // Vision owns model-backed resources and must drop before the weights.
    vision: Option<VisionHandle>,
    // Handles precede their load-time device arrays so weights drop first.
    main: ModelHandle,
    draft: Option<ModelHandle>,
    vocab: *const c_void,
    metadata: LlamaCppModelLoadMetadata,
    _main_devices: SelectedDevices,
    _draft_devices: Option<SelectedDevices>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl LlamaCppModel {
    pub(crate) fn load(config: &LocalInferenceConfig, log: &mut String) -> Result<Self> {
        let mut main_devices = SelectedDevices::from_config(config.runtime.device.as_deref())?;
        let mut main_params = model_params(
            config.runtime.gpu_layers,
            config.runtime.mmap,
            &mut main_devices,
        )?;
        if let Some(device_name) = main_devices.name() {
            log.push_str("selected_device = ");
            log.push_str(device_name);
            log.push('\n');
            main_params.devices = main_devices.as_mut_ptr();
        }

        let model_path = path_cstring(&config.backend.model)?;
        let main = ModelHandle::load(model_path.as_ptr(), main_params).with_context(|| {
            format!(
                "failed to load llama.cpp model {}",
                config.backend.model.display()
            )
        })?;
        let description = main.description();
        log.push_str("model = ");
        log.push_str(&description);
        log.push('\n');

        let vocab = unsafe { ffi::llama_model_get_vocab(main.as_ptr().cast_const()) };
        ensure!(!vocab.is_null(), "llama.cpp model has no vocab");

        let vision = config
            .backend
            .multimodal_projector
            .as_ref()
            .map(|projector| VisionHandle::load(projector, main.as_ptr(), config))
            .transpose()
            .context("failed to initialize llama.cpp multimodal projector")?;
        if vision.is_some() {
            log.push_str("multimodal_projector = loaded\n");
        }

        let (draft, draft_devices, draft_description) = if config.runtime.mtp.enabled {
            let Some(draft_model_path) = &config.runtime.mtp.draft_model else {
                bail!("runtime.mtp enabled requires draft_model");
            };
            let draft_gpu_layers = config
                .runtime
                .mtp
                .gpu_layers
                .unwrap_or(config.runtime.gpu_layers);
            let mut devices = SelectedDevices::from_config(config.runtime.device.as_deref())?;
            let mut params = model_params(draft_gpu_layers, config.runtime.mmap, &mut devices)?;
            if let Some(device_name) = devices.name() {
                log.push_str("mtp_selected_device = ");
                log.push_str(device_name);
                log.push('\n');
                params.devices = devices.as_mut_ptr();
            }
            let draft_model_path_c = path_cstring(draft_model_path)?;
            let draft =
                ModelHandle::load(draft_model_path_c.as_ptr(), params).with_context(|| {
                    format!(
                        "failed to load llama.cpp MTP draft model {}",
                        draft_model_path.display()
                    )
                })?;
            let description = draft.description();
            log.push_str("mtp_draft_model_desc = ");
            log.push_str(&description);
            log.push('\n');
            (Some(draft), Some(devices), Some(description))
        } else {
            (None, None, None)
        };

        Ok(Self {
            key: LlamaCppModelKey::from_config(config),
            vision,
            main,
            draft,
            vocab,
            metadata: LlamaCppModelLoadMetadata {
                description,
                draft_description,
                selected_device: config.runtime.device.clone(),
            },
            _main_devices: main_devices,
            _draft_devices: draft_devices,
            _thread_bound: PhantomData,
        })
    }

    pub(crate) fn matches_config(&self, config: &LocalInferenceConfig) -> bool {
        self.key == LlamaCppModelKey::from_config(config)
    }

    pub(crate) fn metadata(&self) -> &LlamaCppModelLoadMetadata {
        &self.metadata
    }

    pub(crate) fn record_selected_device(&mut self, selected_device: Option<String>) {
        if selected_device.is_some() {
            self.metadata.selected_device = selected_device;
        }
    }

    pub(super) fn main_ptr(&self) -> *mut c_void {
        self.main.as_ptr()
    }

    pub(super) fn draft_ptr(&self) -> Option<*mut c_void> {
        self.draft.as_ref().map(ModelHandle::as_ptr)
    }

    pub(super) fn vocab(&self) -> *const c_void {
        self.vocab
    }

    pub(super) fn vision_marker(&self) -> Option<&str> {
        self.vision.as_ref().map(VisionHandle::marker)
    }

    pub(super) fn eval_vision(
        &self,
        llama_context: *mut c_void,
        prompt: &str,
        images: &[&[u8]],
        batch_size: usize,
    ) -> Result<(ffi::llama_pos, usize)> {
        let vision = self
            .vision
            .as_ref()
            .context("llama.cpp model has no multimodal projector")?;
        vision.eval(llama_context, prompt, images, batch_size)
    }
}

fn model_params(
    gpu_layers: u32,
    mmap: Option<bool>,
    selected_devices: &mut SelectedDevices,
) -> Result<ffi::llama_model_params> {
    let mut params = unsafe { ffi::llama_model_default_params() };
    params.n_gpu_layers = i32::try_from(gpu_layers).context("llama.cpp gpu_layers exceeds i32")?;
    params.split_mode = ffi::LLAMA_SPLIT_MODE_LAYER;
    if let Some(mmap) = mmap {
        params.use_mmap = mmap;
    }
    if selected_devices.name().is_some() {
        params.devices = selected_devices.as_mut_ptr();
    }
    Ok(params)
}

struct SelectedDevices {
    name: Option<String>,
    devices: Vec<ffi::ggml_backend_dev_t>,
}

impl SelectedDevices {
    fn from_config(device_name: Option<&str>) -> Result<Self> {
        let Some(device_name) = device_name else {
            return Ok(Self {
                name: None,
                devices: Vec::new(),
            });
        };

        let device_name_c = CString::new(device_name).context("llama.cpp device contains NUL")?;
        let device = unsafe { ffi::ggml_backend_dev_by_name(device_name_c.as_ptr()) };
        if device.is_null() {
            bail!("configured llama.cpp device {device_name:?} was not found");
        }
        Ok(Self {
            name: Some(device_name.to_string()),
            devices: vec![device, ptr::null_mut()],
        })
    }

    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::ggml_backend_dev_t {
        self.devices.as_mut_ptr()
    }
}

struct ModelHandle(*mut c_void);

impl ModelHandle {
    fn load(path: *const c_char, params: ffi::llama_model_params) -> Result<Self> {
        let model = unsafe { ffi::llama_model_load_from_file(path, params) };
        ensure!(!model.is_null(), "llama.cpp returned null model");
        Ok(Self(model))
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    fn description(&self) -> String {
        let mut buf = vec![0_i8; 512];
        let len =
            unsafe { ffi::llama_model_desc(self.0.cast_const(), buf.as_mut_ptr(), buf.len()) };
        if len <= 0 {
            return "unknown".to_string();
        }
        let len = usize::try_from(len).unwrap_or(0).min(buf.len());
        let bytes = buf[..len]
            .iter()
            .map(|value| *value as u8)
            .collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .to_string()
    }
}

impl Drop for ModelHandle {
    fn drop(&mut self) {
        unsafe { ffi::llama_model_free(self.0) };
    }
}

struct VisionHandle {
    raw: *mut c_void,
    marker: String,
}

impl VisionHandle {
    fn load(
        projector: &std::path::Path,
        model: *mut c_void,
        config: &LocalInferenceConfig,
    ) -> Result<Self> {
        let projector = path_cstring(projector)?;
        let threads = i32::try_from(config.runtime.threads)
            .context("llama.cpp multimodal threads exceeds i32")?;
        let mut error = vec![0_i8; 4096];
        let raw = unsafe {
            ffi::agl_mtmd_init(
                projector.as_ptr(),
                model.cast_const(),
                config.runtime.gpu_layers > 0,
                threads,
                map_flash_attention(config.runtime.flash_attention),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        ensure!(
            !raw.is_null(),
            "llama.cpp mtmd initialization failed: {}",
            c_error_message(&error)
        );
        let marker_ptr = unsafe { ffi::agl_mtmd_marker(raw.cast_const()) };
        ensure!(!marker_ptr.is_null(), "llama.cpp mtmd marker is missing");
        let marker = unsafe { CStr::from_ptr(marker_ptr) }
            .to_str()
            .context("llama.cpp mtmd marker is not UTF-8")?
            .to_string();
        ensure!(!marker.is_empty(), "llama.cpp mtmd marker is empty");
        Ok(Self { raw, marker })
    }

    fn marker(&self) -> &str {
        &self.marker
    }

    fn eval(
        &self,
        llama_context: *mut c_void,
        prompt: &str,
        images: &[&[u8]],
        batch_size: usize,
    ) -> Result<(ffi::llama_pos, usize)> {
        ensure!(!llama_context.is_null(), "llama.cpp context is null");
        ensure!(!images.is_empty(), "llama.cpp mtmd image set is empty");
        ensure!(
            images.iter().all(|image| !image.is_empty()),
            "llama.cpp mtmd image buffer is empty"
        );
        let prompt = CString::new(prompt).context("llama.cpp mtmd prompt contains NUL")?;
        let image_data = images
            .iter()
            .map(|image| image.as_ptr())
            .collect::<Vec<_>>();
        let image_lengths = images.iter().map(|image| image.len()).collect::<Vec<_>>();
        let batch_size =
            i32::try_from(batch_size).context("llama.cpp mtmd batch size exceeds i32")?;
        let mut positions = 0;
        let mut tokens = 0;
        let mut error = vec![0_i8; 4096];
        let status = unsafe {
            ffi::agl_mtmd_eval_images(
                self.raw,
                llama_context,
                prompt.as_ptr(),
                image_data.as_ptr(),
                image_lengths.as_ptr(),
                image_data.len(),
                batch_size,
                &mut positions,
                &mut tokens,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        ensure!(
            status == 0,
            "llama.cpp mtmd evaluation failed ({status}): {}",
            c_error_message(&error)
        );
        ensure!(
            positions > 0,
            "llama.cpp mtmd produced no context positions"
        );
        ensure!(tokens > 0, "llama.cpp mtmd produced no input tokens");
        Ok((positions, tokens))
    }
}

impl Drop for VisionHandle {
    fn drop(&mut self) {
        unsafe { ffi::agl_mtmd_free(self.raw) };
    }
}

fn map_flash_attention(value: Option<RuntimeSwitch>) -> i32 {
    match value {
        Some(RuntimeSwitch::On) => ffi::LLAMA_FLASH_ATTN_TYPE_ENABLED,
        Some(RuntimeSwitch::Off) => ffi::LLAMA_FLASH_ATTN_TYPE_DISABLED,
        Some(RuntimeSwitch::Auto) | None => ffi::LLAMA_FLASH_ATTN_TYPE_AUTO,
    }
}

fn c_error_message(error: &[c_char]) -> String {
    let bytes = error
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(unix)]
fn path_cstring(path: &std::path::Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

#[cfg(test)]
mod tests {
    use agl_config::{
        BackendKind, InferenceBackendConfig, InferenceRuntimeConfig, ModelConfig, ModelDialect,
        MtpRuntimeConfig, PromptConfig, ToolCallFormat,
    };

    use super::*;

    fn config() -> LocalInferenceConfig {
        LocalInferenceConfig {
            backend: InferenceBackendConfig {
                kind: BackendKind::LlamaCpp,
                model: PathBuf::from("/models/main.gguf"),
                multimodal_projector: None,
            },
            runtime: InferenceRuntimeConfig {
                gpu_layers: 24,
                context_tokens: 4096,
                threads: 4,
                device: Some("Vulkan0".to_string()),
                batch_size: Some(512),
                ubatch_size: Some(128),
                flash_attention: None,
                cache_type_k: None,
                cache_type_v: None,
                mmap: Some(true),
                kv_unified: None,
                mtp: MtpRuntimeConfig::default(),
            },
            model: ModelConfig {
                dialect: ModelDialect::Qwen3,
                tool_call_format: ToolCallFormat::HermesJson,
            },
            prompt: PromptConfig::default(),
        }
    }

    #[test]
    fn model_key_ignores_per_context_runtime_settings() {
        let original = config();
        let mut changed = original.clone();
        changed.runtime.context_tokens = 8192;
        changed.runtime.threads = 12;
        changed.runtime.batch_size = Some(1024);

        assert_eq!(
            LlamaCppModelKey::from_config(&original),
            LlamaCppModelKey::from_config(&changed)
        );
    }

    #[test]
    fn model_key_includes_weight_loading_settings() {
        let original = config();
        let mut gpu_changed = original.clone();
        gpu_changed.runtime.gpu_layers += 1;
        let mut draft_changed = original.clone();
        draft_changed.runtime.mtp.enabled = true;
        draft_changed.runtime.mtp.draft_model = Some(PathBuf::from("/models/draft.gguf"));
        draft_changed.runtime.mtp.draft_tokens = 4;

        assert_ne!(
            LlamaCppModelKey::from_config(&original),
            LlamaCppModelKey::from_config(&gpu_changed)
        );
        assert_ne!(
            LlamaCppModelKey::from_config(&original),
            LlamaCppModelKey::from_config(&draft_changed)
        );
    }

    #[test]
    fn model_key_includes_multimodal_projector() {
        let original = config();
        let mut changed = original.clone();
        changed.backend.multimodal_projector = Some(PathBuf::from("/models/mmproj.gguf"));

        assert_ne!(
            LlamaCppModelKey::from_config(&original),
            LlamaCppModelKey::from_config(&changed)
        );
    }
}
