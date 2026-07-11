use std::ffi::{CString, c_char, c_void};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;

use agl_config::LocalInferenceConfig;
use anyhow::{Context, Result, bail, ensure};

use super::ffi;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlamaCppModelKey {
    model: PathBuf,
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
}
