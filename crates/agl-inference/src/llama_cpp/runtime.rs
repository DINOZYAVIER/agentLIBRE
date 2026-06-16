use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use agl_config::{KvCacheType, LocalInferenceConfig, RuntimeSwitch};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};
use anyhow::{Context, Result, bail, ensure};

use crate::InferenceFinishReason;

use super::ffi;

static LLAMA_BACKEND: OnceLock<()> = OnceLock::new();
static LLAMA_LOGS: Mutex<String> = Mutex::new(String::new());

pub(super) struct LlamaCppRuntime {
    config: LocalInferenceConfig,
    max_output_tokens: u32,
}

pub(super) struct LlamaCppRuntimeOutput {
    pub(super) content: String,
    pub(super) finish_reason: InferenceFinishReason,
    pub(super) log: String,
}

impl LlamaCppRuntime {
    pub(super) fn new(config: LocalInferenceConfig, max_output_tokens: u32) -> Self {
        Self {
            config,
            max_output_tokens,
        }
    }

    pub(super) fn generate(
        &mut self,
        rendered: &RenderedModelRequest,
    ) -> Result<LlamaCppRuntimeOutput> {
        ensure!(
            self.max_output_tokens > 0,
            "llama.cpp max_output_tokens cannot be zero"
        );
        init_llama_backend();
        clear_llama_logs();

        let mut log = runtime_log_header();
        let mut model_params = unsafe { ffi::llama_model_default_params() };
        model_params.n_gpu_layers = i32::try_from(self.config.runtime.gpu_layers)
            .context("llama.cpp gpu_layers exceeds i32")?;
        model_params.split_mode = ffi::LLAMA_SPLIT_MODE_LAYER;
        if let Some(mmap) = self.config.runtime.mmap {
            model_params.use_mmap = mmap;
        }

        let mut selected_devices =
            SelectedDevices::from_config(self.config.runtime.device.as_deref())?;
        if let Some(device_name) = selected_devices.name() {
            log.push_str("selected_device = ");
            log.push_str(device_name);
            log.push('\n');
            model_params.devices = selected_devices.as_mut_ptr();
        }

        let model_path = path_cstring(&self.config.backend.model)?;
        let model = Model::load(model_path.as_ptr(), model_params).with_context(|| {
            format!(
                "failed to load llama.cpp model {}",
                self.config.backend.model.display()
            )
        })?;
        log.push_str("model = ");
        log.push_str(&model.description());
        log.push('\n');

        let mut context_params = unsafe { ffi::llama_context_default_params() };
        context_params.n_ctx = self.config.runtime.context_tokens;
        context_params.n_batch = self
            .config
            .runtime
            .batch_size
            .unwrap_or(self.config.runtime.context_tokens);
        if let Some(ubatch_size) = self.config.runtime.ubatch_size {
            context_params.n_ubatch = ubatch_size;
        }
        context_params.n_threads =
            i32::try_from(self.config.runtime.threads).context("llama.cpp threads exceeds i32")?;
        context_params.n_threads_batch = context_params.n_threads;
        context_params.flash_attn_type = map_flash_attention(self.config.runtime.flash_attention);
        if let Some(cache_type) = self.config.runtime.cache_type_k {
            context_params.type_k = map_cache_type(cache_type);
        }
        if let Some(cache_type) = self.config.runtime.cache_type_v {
            context_params.type_v = map_cache_type(cache_type);
        }

        let context = ContextHandle::new(model.as_ptr(), context_params)
            .context("failed to create llama.cpp context")?;
        log.push_str("n_ctx = ");
        log.push_str(&unsafe { ffi::llama_n_ctx(context.as_ptr()) }.to_string());
        log.push('\n');

        let sampler = Sampler::greedy().context("failed to create llama.cpp sampler")?;
        let vocab = unsafe { ffi::llama_model_get_vocab(model.as_ptr().cast_const()) };
        ensure!(!vocab.is_null(), "llama.cpp model has no vocab");

        let mut prompt = apply_chat_template(model.as_ptr().cast_const(), rendered)
            .context("failed to render llama.cpp chat template")?;
        if disable_qwen_thinking(&mut prompt) {
            log.push_str("thinking_prefill = disabled\n");
        }
        log.push_str("llama_cpp_prompt:\n");
        log.push_str(&prompt);
        if !prompt.ends_with('\n') {
            log.push('\n');
        }
        let mut prompt_tokens = tokenize(vocab, &prompt, true)?;
        ensure!(
            !prompt_tokens.is_empty(),
            "llama.cpp prompt produced no tokens"
        );
        ensure!(
            prompt_tokens.len() < unsafe { ffi::llama_n_ctx(context.as_ptr()) } as usize,
            "llama.cpp prompt exceeds context size"
        );
        decode_tokens(context.as_ptr(), &mut prompt_tokens).context("failed to decode prompt")?;

        let mut content = String::new();
        let mut finish_reason = InferenceFinishReason::Length;
        for _ in 0..self.max_output_tokens {
            let token =
                unsafe { ffi::llama_sampler_sample(sampler.as_ptr(), context.as_ptr(), -1) };
            if unsafe { ffi::llama_vocab_is_eog(vocab, token) } {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }

            content.push_str(&token_to_piece(vocab, token)?);
            if truncate_at_stop_marker(&mut content) {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }

            if !has_context_space(context.as_ptr(), 1) {
                finish_reason = InferenceFinishReason::Length;
                break;
            }
            let mut next_token = [token];
            decode_tokens(context.as_ptr(), &mut next_token)
                .context("failed to decode generated token")?;
        }

        Ok(LlamaCppRuntimeOutput {
            content,
            finish_reason,
            log: finish_runtime_log(log),
        })
    }
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

fn finish_runtime_log(mut log: String) -> String {
    let native_logs = LLAMA_LOGS
        .lock()
        .map(|mut logs| std::mem::take(&mut *logs))
        .unwrap_or_default();
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
            bail!(
                "configured llama.cpp device {device_name:?} was not found\navailable devices:\n{}",
                available_devices()
            );
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

struct Model(*mut c_void);

impl Model {
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

impl Drop for Model {
    fn drop(&mut self) {
        unsafe { ffi::llama_model_free(self.0) };
    }
}

struct ContextHandle(*mut c_void);

impl ContextHandle {
    fn new(model: *mut c_void, params: ffi::llama_context_params) -> Result<Self> {
        let context = unsafe { ffi::llama_init_from_model(model, params) };
        ensure!(!context.is_null(), "llama.cpp returned null context");
        Ok(Self(context))
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for ContextHandle {
    fn drop(&mut self) {
        unsafe { ffi::llama_free(self.0) };
    }
}

struct Sampler(*mut c_void);

impl Sampler {
    fn greedy() -> Result<Self> {
        let params = unsafe { ffi::llama_sampler_chain_default_params() };
        let chain = unsafe { ffi::llama_sampler_chain_init(params) };
        ensure!(!chain.is_null(), "llama.cpp returned null sampler chain");
        let greedy = unsafe { ffi::llama_sampler_init_greedy() };
        if greedy.is_null() {
            unsafe { ffi::llama_sampler_free(chain) };
            bail!("llama.cpp returned null greedy sampler");
        }
        unsafe { ffi::llama_sampler_chain_add(chain, greedy) };
        Ok(Self(chain))
    }

    #[allow(dead_code)]
    fn default_sampling() -> Result<Self> {
        let params = unsafe { ffi::llama_sampler_chain_default_params() };
        let chain = unsafe { ffi::llama_sampler_chain_init(params) };
        ensure!(!chain.is_null(), "llama.cpp returned null sampler chain");
        unsafe {
            ffi::llama_sampler_chain_add(chain, ffi::llama_sampler_init_min_p(0.05, 1));
            ffi::llama_sampler_chain_add(chain, ffi::llama_sampler_init_temp(0.8));
            ffi::llama_sampler_chain_add(
                chain,
                ffi::llama_sampler_init_dist(ffi::LLAMA_DEFAULT_SEED),
            );
        }
        Ok(Self(chain))
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe { ffi::llama_sampler_free(self.0) };
    }
}

fn apply_chat_template(model: *const c_void, rendered: &RenderedModelRequest) -> Result<String> {
    let template = unsafe { ffi::llama_model_chat_template(model, ptr::null()) };
    let prepared = PreparedChatMessages::new(&rendered.messages)?;
    let needed = unsafe {
        ffi::llama_chat_apply_template(
            template,
            prepared.messages.as_ptr(),
            prepared.messages.len(),
            true,
            ptr::null_mut(),
            0,
        )
    };
    ensure!(
        needed >= 0,
        "llama.cpp chat template rejected rendered messages"
    );

    let mut buf = vec![0_i8; usize::try_from(needed).unwrap_or(0) + 1];
    let written = unsafe {
        ffi::llama_chat_apply_template(
            template,
            prepared.messages.as_ptr(),
            prepared.messages.len(),
            true,
            buf.as_mut_ptr(),
            i32::try_from(buf.len()).context("llama.cpp prompt exceeds i32")?,
        )
    };
    ensure!(written >= 0, "llama.cpp chat template failed");
    let len = usize::try_from(written).context("llama.cpp returned invalid prompt length")?;
    let bytes = buf[..len]
        .iter()
        .map(|value| *value as u8)
        .collect::<Vec<_>>();
    String::from_utf8(bytes).context("llama.cpp chat template produced invalid UTF-8")
}

struct PreparedChatMessages {
    _roles: Vec<CString>,
    _contents: Vec<CString>,
    messages: Vec<ffi::llama_chat_message>,
}

impl PreparedChatMessages {
    fn new(messages: &[RenderedMessage]) -> Result<Self> {
        let mut roles = Vec::with_capacity(messages.len());
        let mut contents = Vec::with_capacity(messages.len());
        let mut ffi_messages = Vec::with_capacity(messages.len());

        for message in messages {
            let role = CString::new(match message.role {
                RenderedMessageRole::User => "user",
                RenderedMessageRole::Assistant => "assistant",
                RenderedMessageRole::Tool => "tool",
            })?;
            let content = CString::new(rendered_message_content(message)?)?;
            ffi_messages.push(ffi::llama_chat_message {
                role: role.as_ptr(),
                content: content.as_ptr(),
            });
            roles.push(role);
            contents.push(content);
        }

        Ok(Self {
            _roles: roles,
            _contents: contents,
            messages: ffi_messages,
        })
    }
}

pub(crate) fn rendered_message_content(message: &RenderedMessage) -> Result<String> {
    let mut content = message.content.clone();
    for tool_call in &message.tool_calls {
        if !content.is_empty() {
            content.push('\n');
        }
        let payload = serde_json::json!({
            "name": tool_call.name,
            "arguments": tool_call.arguments,
        });
        content.push_str(&serde_json::to_string(&payload).context(
            "failed to serialize rendered structured tool call for llama.cpp chat message",
        )?);
    }
    Ok(content)
}

fn tokenize(vocab: *const c_void, text: &str, add_special: bool) -> Result<Vec<ffi::llama_token>> {
    let text_c = CString::new(text).context("llama.cpp prompt contains NUL")?;
    let text_len = i32::try_from(text.len()).context("llama.cpp prompt exceeds i32")?;
    let required = unsafe {
        ffi::llama_tokenize(
            vocab,
            text_c.as_ptr(),
            text_len,
            ptr::null_mut(),
            0,
            add_special,
            true,
        )
    };
    let token_count = if required < 0 { -required } else { required };
    ensure!(token_count > 0, "llama.cpp tokenization produced no tokens");
    let mut tokens = vec![0; usize::try_from(token_count).context("invalid token count")?];
    let actual = unsafe {
        ffi::llama_tokenize(
            vocab,
            text_c.as_ptr(),
            text_len,
            tokens.as_mut_ptr(),
            token_count,
            add_special,
            true,
        )
    };
    ensure!(actual >= 0, "llama.cpp tokenization failed");
    tokens.truncate(usize::try_from(actual).context("invalid token count")?);
    Ok(tokens)
}

fn decode_tokens(ctx: *mut c_void, tokens: &mut [ffi::llama_token]) -> Result<()> {
    ensure!(
        !tokens.is_empty(),
        "cannot decode empty llama.cpp token batch"
    );
    let n_tokens = i32::try_from(tokens.len()).context("llama.cpp token batch exceeds i32")?;
    let batch = unsafe { ffi::llama_batch_get_one(tokens.as_mut_ptr(), n_tokens) };
    let code = unsafe { ffi::llama_decode(ctx, batch) };
    ensure!(code == 0, "llama.cpp decode failed with code {code}");
    Ok(())
}

fn token_to_piece(vocab: *const c_void, token: ffi::llama_token) -> Result<String> {
    let mut buf = vec![0_i8; 256];
    let len = unsafe {
        ffi::llama_token_to_piece(vocab, token, buf.as_mut_ptr(), buf.len() as i32, 0, false)
    };
    if len < 0 {
        let needed = usize::try_from(-len).context("invalid llama.cpp piece length")? + 1;
        buf.resize(needed, 0);
        let len = unsafe {
            ffi::llama_token_to_piece(vocab, token, buf.as_mut_ptr(), buf.len() as i32, 0, false)
        };
        ensure!(len >= 0, "llama.cpp token_to_piece failed");
        return piece_buf_to_string(&buf, len);
    }
    piece_buf_to_string(&buf, len)
}

fn piece_buf_to_string(buf: &[i8], len: i32) -> Result<String> {
    let len = usize::try_from(len).context("invalid llama.cpp piece length")?;
    let bytes = buf[..len]
        .iter()
        .map(|value| *value as u8)
        .collect::<Vec<_>>();
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn truncate_at_stop_marker(content: &mut String) -> bool {
    let marker_offset = ["\nUser:", "\nAssistant:", "\nTool:", "<|im_end|>"]
        .iter()
        .filter_map(|marker| content.find(marker))
        .min();
    if let Some(offset) = marker_offset {
        content.truncate(offset);
        true
    } else {
        false
    }
}

fn disable_qwen_thinking(prompt: &mut String) -> bool {
    const THINKING_PREFILL: &str = "<think>\n";
    const ASSISTANT_HEADER: &str = "<|im_start|>assistant\n";
    if prompt.ends_with(THINKING_PREFILL) {
        let truncate_to = prompt.len() - THINKING_PREFILL.len();
        prompt.truncate(truncate_to);
        prompt.push_str("<think>\n\n</think>\n\n");
        return true;
    }
    if prompt.ends_with(ASSISTANT_HEADER) {
        prompt.push_str("<think>\n\n</think>\n\n");
        return true;
    }
    false
}

fn has_context_space(ctx: *mut c_void, requested_tokens: i32) -> bool {
    let used = unsafe { ffi::llama_memory_seq_pos_max(ffi::llama_get_memory(ctx), 0) } + 1;
    used.saturating_add(requested_tokens) < unsafe { ffi::llama_n_ctx(ctx) } as i32
}

fn map_flash_attention(value: Option<RuntimeSwitch>) -> i32 {
    match value {
        Some(RuntimeSwitch::On) => ffi::LLAMA_FLASH_ATTN_TYPE_ENABLED,
        Some(RuntimeSwitch::Off) => ffi::LLAMA_FLASH_ATTN_TYPE_DISABLED,
        Some(RuntimeSwitch::Auto) | None => ffi::LLAMA_FLASH_ATTN_TYPE_AUTO,
    }
}

fn map_cache_type(value: KvCacheType) -> i32 {
    match value {
        KvCacheType::F32 => ffi::GGML_TYPE_F32,
        KvCacheType::F16 => ffi::GGML_TYPE_F16,
        KvCacheType::Bf16 => ffi::GGML_TYPE_BF16,
        KvCacheType::Q8_0 => ffi::GGML_TYPE_Q8_0,
        KvCacheType::Q4_0 => ffi::GGML_TYPE_Q4_0,
        KvCacheType::Q4_1 => ffi::GGML_TYPE_Q4_1,
        KvCacheType::Iq4Nl => ffi::GGML_TYPE_IQ4_NL,
        KvCacheType::Q5_0 => ffi::GGML_TYPE_Q5_0,
        KvCacheType::Q5_1 => ffi::GGML_TYPE_Q5_1,
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

#[cfg(unix)]
fn path_cstring(path: &std::path::Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

#[cfg(test)]
mod tests {
    use agl_config::{ModelDialect, ToolCallFormat};
    use agl_oven::{RenderedTool, RenderedToolCall};
    use serde_json::json;

    use super::*;

    #[test]
    fn rendered_message_content_includes_tool_calls() {
        let message = RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: "result".to_string(),
            name: None,
            tool_calls: vec![RenderedToolCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}),
            }],
        };

        let content = rendered_message_content(&message).unwrap();

        assert!(content.contains("result\n"));
        assert!(content.contains("\"name\":\"read_file\""));
        assert!(content.contains("\"path\":\"README.md\""));
    }

    #[test]
    fn stop_marker_truncates_generated_transcript_continuation() {
        let mut content = "hello\n\nUser:\nnext".to_string();

        assert!(truncate_at_stop_marker(&mut content));
        assert_eq!(content, "hello\n");
    }

    #[test]
    fn disables_qwen_thinking_prefill() {
        let mut prompt =
            "<|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\n<think>\n".to_string();

        assert!(disable_qwen_thinking(&mut prompt));
        assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
    }

    #[test]
    fn disables_qwen_thinking_after_plain_assistant_header() {
        let mut prompt = "<|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\n".to_string();

        assert!(disable_qwen_thinking(&mut prompt));
        assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
    }

    #[test]
    fn prepared_chat_messages_use_llama_roles() {
        let rendered = RenderedModelRequest {
            turn_id: "turn".to_string(),
            request_index: 0,
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
            messages: vec![RenderedMessage {
                role: RenderedMessageRole::User,
                content: "hello".to_string(),
                name: None,
                tool_calls: Vec::new(),
            }],
            tools: vec![RenderedTool {
                name: "unused".to_string(),
                required_arguments: Vec::new(),
            }],
        };

        let prepared = PreparedChatMessages::new(&rendered.messages).unwrap();

        assert_eq!(prepared.messages.len(), 1);
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[0].role) }
                .to_str()
                .unwrap(),
            "user"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[0].content) }
                .to_str()
                .unwrap(),
            "hello"
        );
    }
}
