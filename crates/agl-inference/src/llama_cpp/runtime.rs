use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::ptr;
use std::sync::{Mutex, OnceLock};

use agl_config::LocalInferenceConfig;
use agl_oven::RenderedModelRequest;
use anyhow::{Result, ensure};

use crate::InferenceFinishReason;

use super::ffi;
use super::session::{LlamaCppModelState, LlamaCppSession};

#[cfg(test)]
use super::session::trim_generated_continuation;
#[cfg(test)]
use agl_oven::{RenderedMessage, RenderedMessageRole};
#[cfg(test)]
use std::collections::VecDeque;

static LLAMA_BACKEND: OnceLock<()> = OnceLock::new();
static LLAMA_LOGS: Mutex<String> = Mutex::new(String::new());

pub(super) struct LlamaCppRuntime {
    inner: LlamaCppRuntimeInner,
}

enum LlamaCppRuntimeInner {
    Native(NativeLlamaCppRuntime),
    #[cfg(test)]
    Test(TestLlamaCppRuntime),
}

struct NativeLlamaCppRuntime {
    config: LocalInferenceConfig,
    max_output_tokens: u32,
    session: Option<LlamaCppSession>,
}

pub(super) struct LlamaCppRuntimeOutput {
    pub(super) content: String,
    pub(super) finish_reason: InferenceFinishReason,
    pub(super) model_state: String,
    pub(super) selected_device: Option<String>,
    pub(super) log: String,
}

#[derive(Debug)]
pub(super) struct LlamaCppRuntimeError {
    message: String,
    log: String,
}

impl LlamaCppRuntimeError {
    fn new(message: String, log: String) -> Self {
        Self { message, log }
    }

    pub(super) fn log(&self) -> &str {
        &self.log
    }
}

impl fmt::Display for LlamaCppRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for LlamaCppRuntimeError {}

impl LlamaCppRuntime {
    pub(super) fn new(config: LocalInferenceConfig, max_output_tokens: u32) -> Self {
        Self {
            inner: LlamaCppRuntimeInner::Native(NativeLlamaCppRuntime {
                config,
                max_output_tokens,
                session: None,
            }),
        }
    }

    #[cfg(test)]
    pub(super) fn new_test(
        config: LocalInferenceConfig,
        max_output_tokens: u32,
        responses: Vec<&str>,
    ) -> Self {
        Self {
            inner: LlamaCppRuntimeInner::Test(TestLlamaCppRuntime {
                config,
                max_output_tokens,
                responses: responses
                    .into_iter()
                    .map(str::to_string)
                    .collect::<VecDeque<_>>(),
                loaded: false,
                rendered_message_history_len: 0,
            }),
        }
    }

    pub(super) fn config(&self) -> &LocalInferenceConfig {
        match &self.inner {
            LlamaCppRuntimeInner::Native(runtime) => &runtime.config,
            #[cfg(test)]
            LlamaCppRuntimeInner::Test(runtime) => &runtime.config,
        }
    }

    pub(super) fn set_max_output_tokens(&mut self, max_output_tokens: u32) {
        match &mut self.inner {
            LlamaCppRuntimeInner::Native(runtime) => {
                runtime.max_output_tokens = max_output_tokens;
            }
            #[cfg(test)]
            LlamaCppRuntimeInner::Test(runtime) => {
                runtime.max_output_tokens = max_output_tokens;
            }
        }
    }

    pub(super) fn clear_context(&mut self) {
        match &mut self.inner {
            LlamaCppRuntimeInner::Native(runtime) => {
                runtime.session = None;
            }
            #[cfg(test)]
            LlamaCppRuntimeInner::Test(runtime) => {
                runtime.loaded = false;
                runtime.rendered_message_history_len = 0;
            }
        }
    }

    pub(super) fn generate(
        &mut self,
        rendered: &RenderedModelRequest,
    ) -> Result<LlamaCppRuntimeOutput> {
        match &mut self.inner {
            LlamaCppRuntimeInner::Native(runtime) => runtime.generate(rendered),
            #[cfg(test)]
            LlamaCppRuntimeInner::Test(runtime) => runtime.generate(rendered),
        }
    }
}

impl NativeLlamaCppRuntime {
    fn generate(&mut self, rendered: &RenderedModelRequest) -> Result<LlamaCppRuntimeOutput> {
        ensure!(
            self.max_output_tokens > 0,
            "llama.cpp max_output_tokens cannot be zero"
        );
        init_llama_backend();
        clear_llama_logs();

        let mut log = runtime_log_header();
        let model_state = match self.ensure_session(&mut log) {
            Ok(model_state) => model_state,
            Err(err) => {
                return Err(runtime_error(err.to_string(), log));
            }
        };
        log.push_str("model_state = ");
        log.push_str(model_state.as_str());
        log.push('\n');
        if let Some(device) = &self.config.runtime.device {
            log.push_str("selected_device = ");
            log.push_str(device);
            log.push('\n');
        }

        let Some(session) = self.session.as_mut() else {
            return Err(runtime_error(
                "llama.cpp session was not initialized".to_string(),
                log,
            ));
        };
        if model_state == LlamaCppModelState::Reused && !session.load_native_log().is_empty() {
            log.push_str("llama_cpp_session_load_log:\n");
            log.push_str(session.load_native_log());
            if !session.load_native_log().ends_with('\n') {
                log.push('\n');
            }
        }

        let output = match session.generate(rendered, self.max_output_tokens, &mut log) {
            Ok(output) => output,
            Err(err) => {
                return Err(runtime_error(err.to_string(), log));
            }
        };
        let native_logs = take_llama_logs();
        if model_state == LlamaCppModelState::Loaded {
            session.set_load_native_log(native_logs.clone());
        }

        Ok(LlamaCppRuntimeOutput {
            content: output.content,
            finish_reason: output.finish_reason,
            model_state: model_state.as_str().to_string(),
            selected_device: self.config.runtime.device.clone(),
            log: finish_runtime_log(log, native_logs),
        })
    }

    fn ensure_session(&mut self, log: &mut String) -> Result<LlamaCppModelState> {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.matches_config(&self.config))
        {
            return Ok(LlamaCppModelState::Reused);
        }

        self.session = Some(LlamaCppSession::load(&self.config, log)?);
        Ok(LlamaCppModelState::Loaded)
    }
}

fn runtime_error(message: String, log: String) -> anyhow::Error {
    LlamaCppRuntimeError::new(message, finish_runtime_log(log, take_llama_logs())).into()
}

#[cfg(test)]
struct TestLlamaCppRuntime {
    config: LocalInferenceConfig,
    max_output_tokens: u32,
    responses: VecDeque<String>,
    loaded: bool,
    rendered_message_history_len: usize,
}

#[cfg(test)]
impl TestLlamaCppRuntime {
    fn generate(&mut self, rendered: &RenderedModelRequest) -> Result<LlamaCppRuntimeOutput> {
        ensure!(
            self.max_output_tokens > 0,
            "llama.cpp max_output_tokens cannot be zero"
        );

        let model_state = if self.loaded {
            LlamaCppModelState::Reused
        } else {
            self.loaded = true;
            LlamaCppModelState::Loaded
        };
        let mut content = self
            .responses
            .pop_front()
            .unwrap_or_else(|| "test response".to_string());
        trim_generated_continuation(&mut content);

        ensure!(
            rendered.messages.len() >= self.rendered_message_history_len,
            "llama.cpp session cannot append {} rendered messages after {} were recorded",
            rendered.messages.len(),
            self.rendered_message_history_len
        );
        let appended_messages = &rendered.messages[self.rendered_message_history_len..];
        let mut log = test_runtime_log(
            &self.config,
            model_state,
            self.rendered_message_history_len,
            appended_messages,
        );
        self.rendered_message_history_len = rendered.messages.len() + 1;
        if model_state == LlamaCppModelState::Reused {
            log.push_str("llama_cpp_session_load_log:\n");
            log.push_str("load_tensors: offloaded 66/66 layers to GPU\n");
        }

        Ok(LlamaCppRuntimeOutput {
            content,
            finish_reason: InferenceFinishReason::Stop,
            model_state: model_state.as_str().to_string(),
            selected_device: self.config.runtime.device.clone(),
            log,
        })
    }
}

#[cfg(test)]
fn test_runtime_log(
    config: &LocalInferenceConfig,
    model_state: LlamaCppModelState,
    rendered_message_history_len: usize,
    appended_messages: &[RenderedMessage],
) -> String {
    let mut log = String::new();
    log.push_str("backend = llama_cpp\n");
    log.push_str("model_state = ");
    log.push_str(model_state.as_str());
    log.push('\n');
    if let Some(device) = &config.runtime.device {
        log.push_str("selected_device = ");
        log.push_str(device);
        log.push('\n');
    }
    log.push_str("load_tensors: offloaded 66/66 layers to GPU\n");
    log.push_str("rendered_message_history_len = ");
    log.push_str(&rendered_message_history_len.to_string());
    log.push('\n');
    if appended_messages
        .last()
        .is_some_and(|message| message.role == RenderedMessageRole::User)
    {
        log.push_str("thinking_prefill = disabled\n");
    }
    log.push_str("llama_cpp_prompt_append:\n");
    for message in appended_messages {
        write_test_message(&mut log, message);
    }
    log
}

#[cfg(test)]
fn write_test_message(log: &mut String, message: &RenderedMessage) {
    match message.role {
        RenderedMessageRole::User => log.push_str("User: "),
        RenderedMessageRole::Assistant => log.push_str("Assistant: "),
        RenderedMessageRole::Tool => log.push_str("Tool: "),
    }
    log.push_str(&message.content);
    log.push('\n');
}

fn init_llama_backend() {
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
        && let Ok(mut logs) = LLAMA_LOGS.lock()
    {
        logs.push_str(&text);
    }
}

fn clear_llama_logs() {
    if let Ok(mut logs) = LLAMA_LOGS.lock() {
        logs.clear();
    }
}

fn take_llama_logs() -> String {
    LLAMA_LOGS
        .lock()
        .map(|mut logs| std::mem::take(&mut *logs))
        .unwrap_or_default()
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

fn runtime_log_header() -> String {
    let mut log = String::new();
    log.push_str("backend = llama_cpp\n");
    log.push_str("library_dir = ");
    log.push_str(env!("AGL_LLAMA_CPP_LIBRARY_DIR"));
    log.push('\n');
    log.push_str("supports_gpu_offload = ");
    log.push_str(if unsafe { ffi::llama_supports_gpu_offload() } {
        "true"
    } else {
        "false"
    });
    log.push('\n');
    log.push_str("devices:\n");
    log.push_str(&available_devices());
    if let Some(system_info) = cstr_to_string(unsafe { ffi::llama_print_system_info() }) {
        log.push_str("system_info = ");
        log.push_str(&system_info);
        log.push('\n');
    }
    log
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
