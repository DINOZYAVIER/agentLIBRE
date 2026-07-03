use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::ptr;

use agl_config::{InferenceRuntimeConfig, KvCacheType, LocalInferenceConfig, RuntimeSwitch};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};
use anyhow::{Context, Result, bail, ensure};

use crate::InferenceFinishReason;

use super::ffi;

const DISABLED_THINKING_PREFILL: &str = "<think>\n\n</think>\n\n";
const AGL_LLAMA_MTP_OK: i32 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LlamaCppModelState {
    Loaded,
    Reused,
}

impl LlamaCppModelState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Reused => "reused",
        }
    }
}

pub(super) struct LlamaCppSession {
    key: LlamaCppSessionKey,
    sampler: Sampler,
    mtp: Option<MtpState>,
    context: ContextHandle,
    model: Model,
    vocab: *const c_void,
    rendered_message_history_len: usize,
    formatted_prompt_prefix_len: usize,
    messages: Vec<RenderedMessage>,
    token_history: Vec<ffi::llama_token>,
    load_native_log: String,
    prefill_batch_size: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LlamaCppSessionKey {
    model: PathBuf,
    runtime: InferenceRuntimeConfig,
}

pub(super) struct LlamaCppSessionOutput {
    pub(super) content: String,
    pub(super) finish_reason: InferenceFinishReason,
}

impl LlamaCppSession {
    pub(super) fn load(config: &LocalInferenceConfig, log: &mut String) -> Result<Self> {
        let mut model_params = unsafe { ffi::llama_model_default_params() };
        model_params.n_gpu_layers =
            i32::try_from(config.runtime.gpu_layers).context("llama.cpp gpu_layers exceeds i32")?;
        model_params.split_mode = ffi::LLAMA_SPLIT_MODE_LAYER;
        if let Some(mmap) = config.runtime.mmap {
            model_params.use_mmap = mmap;
        }

        let mut selected_devices = SelectedDevices::from_config(config.runtime.device.as_deref())?;
        if let Some(device_name) = selected_devices.name() {
            log.push_str("selected_device = ");
            log.push_str(device_name);
            log.push('\n');
            model_params.devices = selected_devices.as_mut_ptr();
        }

        let model_path = path_cstring(&config.backend.model)?;
        let model = Model::load(model_path.as_ptr(), model_params).with_context(|| {
            format!(
                "failed to load llama.cpp model {}",
                config.backend.model.display()
            )
        })?;
        log.push_str("model = ");
        log.push_str(&model.description());
        log.push('\n');

        let mut context_params = unsafe { ffi::llama_context_default_params() };
        context_params.n_ctx = config.runtime.context_tokens;
        let prefill_batch_size = config
            .runtime
            .batch_size
            .unwrap_or(config.runtime.context_tokens);
        context_params.n_batch = prefill_batch_size;
        if let Some(ubatch_size) = config.runtime.ubatch_size {
            context_params.n_ubatch = ubatch_size;
        }
        context_params.n_threads =
            i32::try_from(config.runtime.threads).context("llama.cpp threads exceeds i32")?;
        context_params.n_threads_batch = context_params.n_threads;
        context_params.flash_attn_type = map_flash_attention(config.runtime.flash_attention);
        if let Some(cache_type) = config.runtime.cache_type_k {
            context_params.type_k = map_cache_type(cache_type);
        }
        if let Some(cache_type) = config.runtime.cache_type_v {
            context_params.type_v = map_cache_type(cache_type);
        }
        if config.runtime.mtp.enabled {
            context_params.n_outputs_max = config.runtime.mtp.draft_tokens.saturating_add(1);
            context_params.n_rs_seq = config.runtime.mtp.draft_tokens;
        }

        let context = ContextHandle::new(model.as_ptr(), context_params)
            .context("failed to create llama.cpp context")?;
        log.push_str("n_ctx = ");
        log.push_str(&unsafe { ffi::llama_n_ctx(context.as_ptr()) }.to_string());
        log.push('\n');

        let sampler = Sampler::greedy().context("failed to create llama.cpp sampler")?;
        let vocab = unsafe { ffi::llama_model_get_vocab(model.as_ptr().cast_const()) };
        ensure!(!vocab.is_null(), "llama.cpp model has no vocab");
        let prefill_batch_size =
            usize::try_from(prefill_batch_size).context("llama.cpp n_batch exceeds usize")?;
        ensure!(
            prefill_batch_size > 0,
            "llama.cpp n_batch must be greater than zero"
        );
        let mtp = if config.runtime.mtp.enabled {
            Some(
                MtpState::load(config, context.as_ptr(), prefill_batch_size, log)
                    .context("failed to initialize llama.cpp MTP state")?,
            )
        } else {
            None
        };

        Ok(Self {
            key: LlamaCppSessionKey {
                model: config.backend.model.clone(),
                runtime: config.runtime.clone(),
            },
            sampler,
            mtp,
            context,
            model,
            vocab,
            rendered_message_history_len: 0,
            formatted_prompt_prefix_len: 0,
            messages: Vec::new(),
            token_history: Vec::new(),
            load_native_log: String::new(),
            prefill_batch_size,
        })
    }

    pub(super) fn matches_config(&self, config: &LocalInferenceConfig) -> bool {
        self.key.model == config.backend.model && self.key.runtime == config.runtime
    }

    pub(super) fn load_native_log(&self) -> &str {
        &self.load_native_log
    }

    pub(super) fn can_append_rendered(&self, rendered: &RenderedModelRequest) -> bool {
        rendered.messages.len() >= self.rendered_message_history_len
            && self.messages.len() >= self.rendered_message_history_len
            && self.messages[..self.rendered_message_history_len]
                == rendered.messages[..self.rendered_message_history_len]
    }

    pub(super) fn set_load_native_log(&mut self, log: String) {
        self.load_native_log = log;
    }

    pub(super) fn generate(
        &mut self,
        rendered: &RenderedModelRequest,
        max_output_tokens: u32,
        log: &mut String,
    ) -> Result<LlamaCppSessionOutput> {
        let PreparedPrompt {
            text: prompt,
            assistant_context_prefix,
            messages: prompt_messages,
        } = self.prepare_prompt_append(rendered, log)?;

        let add_special = is_context_empty(self.context.as_ptr());
        let mut prompt_tokens = tokenize(self.vocab, &prompt, add_special)?;
        ensure!(
            !prompt_tokens.is_empty(),
            "llama.cpp prompt produced no tokens"
        );
        let prompt_token_count = i32::try_from(prompt_tokens.len())
            .context("llama.cpp prompt token count exceeds i32")?;
        ensure!(
            has_context_space(self.context.as_ptr(), prompt_token_count),
            "llama.cpp prompt exceeds remaining context"
        );
        if self.mtp.is_some() {
            return self.generate_with_mtp(
                rendered,
                prompt_tokens,
                prompt_messages,
                assistant_context_prefix,
                max_output_tokens,
                log,
            );
        }

        decode_prompt_tokens(
            self.context.as_ptr(),
            &mut prompt_tokens,
            self.prefill_batch_size,
            log,
        )
        .context("failed to decode prompt")?;
        self.token_history.extend_from_slice(&prompt_tokens);

        let mut content = String::new();
        let mut finish_reason = InferenceFinishReason::Length;
        for _ in 0..max_output_tokens {
            if !has_context_space(self.context.as_ptr(), 1) {
                finish_reason = InferenceFinishReason::Length;
                break;
            }
            let token = unsafe {
                ffi::llama_sampler_sample(self.sampler.as_ptr(), self.context.as_ptr(), -1)
            };
            if unsafe { ffi::llama_vocab_is_eog(self.vocab, token) } {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }

            let piece = token_to_piece(self.vocab, token)?;
            let mut next_token = [token];
            decode_tokens(self.context.as_ptr(), &mut next_token)
                .context("failed to decode generated token")?;
            self.token_history.push(token);
            content.push_str(&piece);
            if trim_generated_continuation(&mut content) {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }
        }

        self.record_generated_assistant(
            rendered,
            prompt_messages,
            assistant_context_prefix,
            &content,
        )?;

        Ok(LlamaCppSessionOutput {
            content,
            finish_reason,
        })
    }

    fn generate_with_mtp(
        &mut self,
        rendered: &RenderedModelRequest,
        prompt_tokens: Vec<ffi::llama_token>,
        prompt_messages: Vec<RenderedMessage>,
        assistant_context_prefix: String,
        max_output_tokens: u32,
        log: &mut String,
    ) -> Result<LlamaCppSessionOutput> {
        log.push_str("mtp_generation_mode = draft-mtp\n");
        log.push_str("mtp_sequence_mode = seq0-temporary\n");

        let Some(mut mtp) = self.mtp.take() else {
            bail!("llama.cpp MTP state is missing");
        };
        let Some(mut id_last) = prompt_tokens.last().copied() else {
            bail!("llama.cpp MTP prompt produced no tokens");
        };

        let prefill_len = prompt_tokens.len().saturating_sub(1);
        if prefill_len > 0 {
            let mut start_pos = context_next_pos(self.context.as_ptr());
            let mtp_prefill_batch_size = self.prefill_batch_size.min(mtp.draft_tokens + 1);
            let chunk_count = prefill_chunk_count(prefill_len, mtp_prefill_batch_size)?;
            log.push_str("prompt_tokens = ");
            log.push_str(&prompt_tokens.len().to_string());
            log.push('\n');
            log.push_str("mtp_prefill_tokens = ");
            log.push_str(&prefill_len.to_string());
            log.push('\n');
            log.push_str("prefill_batch_size = ");
            log.push_str(&mtp_prefill_batch_size.to_string());
            log.push('\n');
            log.push_str("prefill_chunks = ");
            log.push_str(&chunk_count.to_string());
            log.push('\n');

            for (chunk_index, chunk) in prompt_tokens[..prefill_len]
                .chunks(mtp_prefill_batch_size)
                .enumerate()
            {
                let mut batch =
                    decode_explicit_tokens(self.context.as_ptr(), chunk, start_pos, true)
                        .with_context(|| {
                            format!(
                                "failed to decode MTP prompt chunk {}/{}",
                                chunk_index + 1,
                                chunk_count
                            )
                        })?;
                mtp.process(&mut batch)
                    .context("failed to process MTP prompt batch")?;
                self.token_history.extend_from_slice(chunk);
                start_pos += i32::try_from(chunk.len()).context("MTP prompt chunk too large")?;
            }
        } else {
            log.push_str("prompt_tokens = 1\n");
            log.push_str("mtp_prefill_tokens = 0\n");
            log.push_str("prefill_chunks = 0\n");
        }

        mtp.begin(&self.token_history)
            .context("failed to begin MTP speculation")?;

        let mut content = String::new();
        let mut finish_reason = InferenceFinishReason::Length;
        let mut emitted = 0_u32;
        let mut pending_needs_flush = false;
        let mut stopped_on_eog = false;

        while emitted < max_output_tokens {
            if context_remaining(self.context.as_ptr()) < 2 {
                finish_reason = InferenceFinishReason::Length;
                break;
            }

            let remaining_output = max_output_tokens - emitted;
            let can_use_draft = remaining_output
                > u32::try_from(mtp.draft_tokens).context("MTP draft token count exceeds u32")?
                && context_remaining(self.context.as_ptr())
                    > i32::try_from(mtp.draft_tokens + 1)
                        .context("MTP draft token count exceeds i32")?;
            let draft = if can_use_draft {
                mtp.draft(
                    history_len_as_pos(&self.token_history)?,
                    id_last,
                    &self.token_history,
                )
                .context("failed to draft MTP tokens")?
            } else {
                Vec::new()
            };

            let n_past = history_len_as_pos(&self.token_history)?;
            let mut verify_tokens = Vec::with_capacity(draft.len() + 1);
            verify_tokens.push(id_last);
            verify_tokens.extend_from_slice(&draft);
            let mut batch =
                decode_explicit_tokens(self.context.as_ptr(), &verify_tokens, n_past, true)
                    .context("failed to decode MTP target verification batch")?;
            mtp.process(&mut batch)
                .context("failed to process MTP target verification batch")?;

            let accepted =
                sample_verified_tokens(self.sampler.as_ptr(), self.context.as_ptr(), &draft);
            ensure!(
                !accepted.is_empty(),
                "llama.cpp MTP target verification produced no accepted tokens"
            );
            let n_accepted = accepted.len().saturating_sub(1);
            rollback_rejected_mtp_tokens(self.context.as_ptr(), n_past, draft.len(), n_accepted)?;
            if !draft.is_empty() {
                mtp.accept(
                    u16::try_from(n_accepted).context("accepted MTP draft count exceeds u16")?,
                )
                .context("failed to accept MTP draft tokens")?;
            }

            self.token_history.push(id_last);
            self.token_history
                .extend(accepted.iter().take(n_accepted).copied());
            pending_needs_flush = true;

            for token in &accepted {
                id_last = *token;
                if unsafe { ffi::llama_vocab_is_eog(self.vocab, id_last) } {
                    finish_reason = InferenceFinishReason::Stop;
                    stopped_on_eog = true;
                    pending_needs_flush = false;
                    break;
                }

                content.push_str(&token_to_piece(self.vocab, id_last)?);
                emitted += 1;
                if trim_generated_continuation(&mut content) {
                    finish_reason = InferenceFinishReason::Stop;
                    break;
                }
                if emitted >= max_output_tokens {
                    finish_reason = InferenceFinishReason::Length;
                    break;
                }
            }

            if finish_reason == InferenceFinishReason::Stop || stopped_on_eog {
                break;
            }
        }

        if pending_needs_flush && !stopped_on_eog {
            flush_mtp_pending_token(
                self.context.as_ptr(),
                &mut mtp,
                &mut self.token_history,
                id_last,
            )
            .context("failed to flush final MTP token")?;
        }

        mtp.write_stats_log(log);
        self.mtp = Some(mtp);

        self.record_generated_assistant(
            rendered,
            prompt_messages,
            assistant_context_prefix,
            &content,
        )?;

        Ok(LlamaCppSessionOutput {
            content,
            finish_reason,
        })
    }

    fn prepare_prompt_append(
        &mut self,
        rendered: &RenderedModelRequest,
        log: &mut String,
    ) -> Result<PreparedPrompt> {
        if rendered.messages.len() < self.rendered_message_history_len {
            bail!(
                "llama.cpp session cannot append {} rendered messages after {} were recorded",
                rendered.messages.len(),
                self.rendered_message_history_len
            );
        }
        if !self.can_append_rendered(rendered) {
            bail!("llama.cpp session rendered history prefix changed");
        }

        let mut messages = self.messages.clone();
        messages.extend(
            rendered.messages[self.rendered_message_history_len..]
                .iter()
                .cloned(),
        );

        let mut formatted =
            apply_chat_template_messages(self.model.as_ptr().cast_const(), &messages, true)
                .context("failed to render llama.cpp chat template")?;
        let assistant_context_prefix = disable_qwen_thinking(&mut formatted)
            .map(str::to_string)
            .unwrap_or_default();
        if !assistant_context_prefix.is_empty() {
            log.push_str("thinking_prefill = disabled\n");
        }

        let prompt = formatted
            .get(self.formatted_prompt_prefix_len..)
            .context("llama.cpp formatted prompt prefix is not a UTF-8 boundary")?
            .to_string();
        ensure!(!prompt.is_empty(), "llama.cpp prompt append is empty");

        log.push_str("rendered_message_history_len = ");
        log.push_str(&self.rendered_message_history_len.to_string());
        log.push('\n');
        log.push_str("formatted_prompt_prefix_len = ");
        log.push_str(&self.formatted_prompt_prefix_len.to_string());
        log.push('\n');
        log.push_str("llama_cpp_prompt_append:\n");
        log.push_str(&prompt);
        if !prompt.ends_with('\n') {
            log.push('\n');
        }

        Ok(PreparedPrompt {
            text: prompt,
            assistant_context_prefix,
            messages,
        })
    }

    fn record_generated_assistant(
        &mut self,
        rendered: &RenderedModelRequest,
        mut messages: Vec<RenderedMessage>,
        assistant_context_prefix: String,
        content: &str,
    ) -> Result<()> {
        messages.push(RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: format!("{assistant_context_prefix}{content}"),
            name: None,
            tool_calls: Vec::new(),
        });
        self.messages = messages;
        self.rendered_message_history_len = rendered.messages.len() + 1;
        self.formatted_prompt_prefix_len =
            apply_chat_template_messages(self.model.as_ptr().cast_const(), &self.messages, false)
                .context("failed to render llama.cpp session prefix")?
                .len();
        Ok(())
    }
}

struct PreparedPrompt {
    text: String,
    assistant_context_prefix: String,
    messages: Vec<RenderedMessage>,
}

struct MtpState {
    speculative: MtpSpeculative,
    _draft_context: ContextHandle,
    _draft_model: Model,
    draft_tokens: usize,
}

impl MtpState {
    fn load(
        config: &LocalInferenceConfig,
        target_context: *mut c_void,
        prefill_batch_size: usize,
        log: &mut String,
    ) -> Result<Self> {
        let Some(draft_model_path) = &config.runtime.mtp.draft_model else {
            bail!("runtime.mtp enabled requires draft_model");
        };
        ensure!(
            config.runtime.mtp.draft_tokens > 0,
            "runtime.mtp draft_tokens must be greater than zero"
        );
        let draft_tokens = usize::try_from(config.runtime.mtp.draft_tokens)
            .context("runtime.mtp draft_tokens exceeds usize")?;

        let mut model_params = unsafe { ffi::llama_model_default_params() };
        model_params.n_gpu_layers = i32::try_from(
            config
                .runtime
                .mtp
                .gpu_layers
                .unwrap_or(config.runtime.gpu_layers),
        )
        .context("llama.cpp MTP gpu_layers exceeds i32")?;
        model_params.split_mode = ffi::LLAMA_SPLIT_MODE_LAYER;
        if let Some(mmap) = config.runtime.mmap {
            model_params.use_mmap = mmap;
        }

        let mut selected_devices = SelectedDevices::from_config(config.runtime.device.as_deref())?;
        if let Some(device_name) = selected_devices.name() {
            log.push_str("mtp_selected_device = ");
            log.push_str(device_name);
            log.push('\n');
            model_params.devices = selected_devices.as_mut_ptr();
        }

        let draft_model_path_c = path_cstring(draft_model_path)?;
        let draft_model =
            Model::load(draft_model_path_c.as_ptr(), model_params).with_context(|| {
                format!(
                    "failed to load llama.cpp MTP draft model {}",
                    draft_model_path.display()
                )
            })?;
        log.push_str("mtp_draft_model_desc = ");
        log.push_str(&draft_model.description());
        log.push('\n');

        let mut context_params = unsafe { ffi::llama_context_default_params() };
        context_params.n_ctx = config.runtime.context_tokens;
        context_params.n_batch =
            u32::try_from(prefill_batch_size).context("llama.cpp MTP n_batch exceeds u32")?;
        if let Some(ubatch_size) = config.runtime.ubatch_size {
            context_params.n_ubatch = ubatch_size;
        }
        context_params.n_threads =
            i32::try_from(config.runtime.threads).context("llama.cpp MTP threads exceeds i32")?;
        context_params.n_threads_batch = context_params.n_threads;
        context_params.flash_attn_type = map_flash_attention(config.runtime.flash_attention);
        context_params.ctx_type = ffi::LLAMA_CONTEXT_TYPE_MTP;
        context_params.n_rs_seq = 0;
        context_params.n_outputs_max = 1;
        context_params.ctx_other = target_context;
        if let Some(cache_type) = config
            .runtime
            .mtp
            .cache_type_k
            .or(config.runtime.cache_type_k)
        {
            context_params.type_k = map_cache_type(cache_type);
        }
        if let Some(cache_type) = config
            .runtime
            .mtp
            .cache_type_v
            .or(config.runtime.cache_type_v)
        {
            context_params.type_v = map_cache_type(cache_type);
        }

        let draft_context = ContextHandle::new(draft_model.as_ptr(), context_params)
            .context("failed to create llama.cpp MTP draft context")?;
        let speculative = MtpSpeculative::new(
            target_context,
            draft_context.as_ptr(),
            i32::try_from(draft_tokens).context("runtime.mtp draft_tokens exceeds i32")?,
            config.runtime.mtp.p_min.as_f32(),
        )?;

        log.push_str("mtp_runtime_state = active\n");
        log.push_str("mtp_sequence_mode = seq0-temporary\n");

        Ok(Self {
            speculative,
            _draft_context: draft_context,
            _draft_model: draft_model,
            draft_tokens,
        })
    }

    fn begin(&mut self, prompt_tokens: &[ffi::llama_token]) -> Result<()> {
        self.speculative.begin(prompt_tokens)
    }

    fn process(&mut self, batch: &mut LlamaTokenBatch) -> Result<()> {
        self.speculative.process(batch)
    }

    fn draft(
        &mut self,
        n_past: ffi::llama_pos,
        id_last: ffi::llama_token,
        prompt_tokens: &[ffi::llama_token],
    ) -> Result<Vec<ffi::llama_token>> {
        self.speculative
            .draft(n_past, id_last, prompt_tokens, self.draft_tokens)
    }

    fn accept(&mut self, n_accepted: u16) -> Result<()> {
        self.speculative.accept(n_accepted)
    }

    fn write_stats_log(&self, log: &mut String) {
        let stats = self.speculative.stats();
        log.push_str("mtp_draft_calls = ");
        log.push_str(&stats.draft_calls.to_string());
        log.push('\n');
        log.push_str("mtp_empty_drafts = ");
        log.push_str(&stats.empty_drafts.to_string());
        log.push('\n');
        log.push_str("mtp_drafted_tokens = ");
        log.push_str(&stats.drafted_tokens.to_string());
        log.push('\n');
        log.push_str("mtp_accepted_tokens = ");
        log.push_str(&stats.accepted_tokens.to_string());
        log.push('\n');
        let acceptance_rate = if stats.drafted_tokens == 0 {
            0.0
        } else {
            stats.accepted_tokens as f64 / stats.drafted_tokens as f64
        };
        log.push_str("mtp_acceptance_rate = ");
        log.push_str(&format!("{acceptance_rate:.3}"));
        log.push('\n');
    }
}

struct MtpSpeculative(*mut c_void);

impl MtpSpeculative {
    fn new(
        target_context: *mut c_void,
        draft_context: *mut c_void,
        draft_tokens: i32,
        p_min: f32,
    ) -> Result<Self> {
        let mut error = vec![0_i8; 4096];
        let raw = unsafe {
            ffi::agl_llama_mtp_init(
                target_context,
                draft_context,
                draft_tokens,
                0,
                p_min,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        ensure!(
            !raw.is_null(),
            "llama.cpp MTP speculative init failed: {}",
            c_error_message(&error)
        );
        Ok(Self(raw))
    }

    fn begin(&mut self, prompt_tokens: &[ffi::llama_token]) -> Result<()> {
        mtp_status_to_result(
            unsafe {
                ffi::agl_llama_mtp_begin(self.0, prompt_tokens.as_ptr(), prompt_tokens.len())
            },
            "begin",
        )
    }

    fn process(&mut self, batch: &mut LlamaTokenBatch) -> Result<()> {
        let ffi_batch = batch.as_ffi();
        mtp_status_to_result(
            unsafe { ffi::agl_llama_mtp_process(self.0, ptr::from_ref(&ffi_batch)) },
            "process",
        )
    }

    fn draft(
        &mut self,
        n_past: ffi::llama_pos,
        id_last: ffi::llama_token,
        prompt_tokens: &[ffi::llama_token],
        draft_tokens: usize,
    ) -> Result<Vec<ffi::llama_token>> {
        let mut output = vec![0; draft_tokens];
        let mut output_count = 0_usize;
        mtp_status_to_result(
            unsafe {
                ffi::agl_llama_mtp_draft(
                    self.0,
                    n_past,
                    id_last,
                    prompt_tokens.as_ptr(),
                    prompt_tokens.len(),
                    output.as_mut_ptr(),
                    output.len(),
                    &mut output_count,
                )
            },
            "draft",
        )?;
        ensure!(
            output_count <= output.len(),
            "llama.cpp MTP draft exceeded output capacity"
        );
        output.truncate(output_count);
        Ok(output)
    }

    fn accept(&mut self, n_accepted: u16) -> Result<()> {
        mtp_status_to_result(
            unsafe { ffi::agl_llama_mtp_accept(self.0, n_accepted) },
            "accept",
        )
    }

    fn stats(&self) -> ffi::agl_llama_mtp_stats {
        unsafe { ffi::agl_llama_mtp_get_stats(self.0.cast_const()) }
    }
}

impl Drop for MtpSpeculative {
    fn drop(&mut self) {
        unsafe { ffi::agl_llama_mtp_free(self.0) };
    }
}

fn mtp_status_to_result(status: i32, operation: &str) -> Result<()> {
    if status == AGL_LLAMA_MTP_OK {
        return Ok(());
    }

    let reason = match status {
        1 => "invalid argument",
        2 => "initialization failed",
        3 => "decode failed",
        4 => "output overflow",
        5 => "exception",
        _ => "unknown status",
    };
    bail!("llama.cpp MTP {operation} failed: {reason} ({status})")
}

struct LlamaTokenBatch {
    tokens: Vec<ffi::llama_token>,
    positions: Vec<ffi::llama_pos>,
    n_seq_ids: Vec<i32>,
    _seq_ids: Vec<[ffi::llama_seq_id; 1]>,
    seq_id_ptrs: Vec<*mut ffi::llama_seq_id>,
    logits: Vec<i8>,
}

impl LlamaTokenBatch {
    fn new(tokens: &[ffi::llama_token], start_pos: ffi::llama_pos, logits: bool) -> Result<Self> {
        ensure!(
            !tokens.is_empty(),
            "cannot decode empty llama.cpp token batch"
        );
        let mut positions = Vec::with_capacity(tokens.len());
        for offset in 0..tokens.len() {
            positions.push(
                start_pos
                    .checked_add(
                        i32::try_from(offset).context("llama.cpp batch offset exceeds i32")?,
                    )
                    .context("llama.cpp batch position overflow")?,
            );
        }

        let n_seq_ids = vec![1; tokens.len()];
        let mut seq_ids = vec![[0]; tokens.len()];
        let seq_id_ptrs = seq_ids.iter_mut().map(|ids| ids.as_mut_ptr()).collect();
        let logits = vec![if logits { 1 } else { 0 }; tokens.len()];

        Ok(Self {
            tokens: tokens.to_vec(),
            positions,
            n_seq_ids,
            _seq_ids: seq_ids,
            seq_id_ptrs,
            logits,
        })
    }

    fn as_ffi(&mut self) -> ffi::llama_batch {
        ffi::llama_batch {
            n_tokens: i32::try_from(self.tokens.len()).unwrap_or(i32::MAX),
            token: self.tokens.as_mut_ptr(),
            embd: ptr::null_mut(),
            pos: self.positions.as_mut_ptr(),
            n_seq_id: self.n_seq_ids.as_mut_ptr(),
            seq_id: self.seq_id_ptrs.as_mut_ptr(),
            logits: self.logits.as_mut_ptr(),
        }
    }
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

    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe { ffi::llama_sampler_free(self.0) };
    }
}

fn apply_chat_template_messages(
    model: *const c_void,
    messages: &[RenderedMessage],
    add_assistant: bool,
) -> Result<String> {
    let prepared = PreparedChatMessages::new(messages)?;

    apply_legacy_chat_template(model, &prepared, add_assistant).or_else(|legacy_err| {
        apply_common_chat_template(model, &prepared, add_assistant)
            .with_context(|| format!("legacy llama.cpp chat template path failed: {legacy_err:#}"))
    })
}

fn apply_legacy_chat_template(
    model: *const c_void,
    prepared: &PreparedChatMessages,
    add_assistant: bool,
) -> Result<String> {
    let template = unsafe { ffi::llama_model_chat_template(model, ptr::null()) };
    let needed = unsafe {
        ffi::llama_chat_apply_template(
            template,
            prepared.messages.as_ptr(),
            prepared.messages.len(),
            add_assistant,
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
            add_assistant,
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

fn apply_common_chat_template(
    model: *const c_void,
    prepared: &PreparedChatMessages,
    add_assistant: bool,
) -> Result<String> {
    let mut error = vec![0_i8; 4096];
    let needed = unsafe {
        ffi::agl_llama_common_chat_apply_template(
            model,
            prepared.messages.as_ptr(),
            prepared.messages.len(),
            add_assistant,
            ptr::null_mut(),
            0,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    ensure!(
        needed >= 0,
        "llama.cpp common chat template failed: {}",
        c_error_message(&error)
    );

    let mut buf = vec![0_i8; usize::try_from(needed).unwrap_or(0) + 1];
    let written = unsafe {
        ffi::agl_llama_common_chat_apply_template(
            model,
            prepared.messages.as_ptr(),
            prepared.messages.len(),
            add_assistant,
            buf.as_mut_ptr(),
            buf.len(),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    ensure!(
        written >= 0,
        "llama.cpp common chat template failed: {}",
        c_error_message(&error)
    );
    let len = usize::try_from(written).context("llama.cpp returned invalid prompt length")?;
    let bytes = buf[..len]
        .iter()
        .map(|value| *value as u8)
        .collect::<Vec<_>>();
    String::from_utf8(bytes).context("llama.cpp common chat template produced invalid UTF-8")
}

fn c_error_message(buf: &[c_char]) -> String {
    if buf.first().copied().unwrap_or_default() == 0 {
        return "unknown error".to_string();
    }

    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
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
                RenderedMessageRole::System => "system",
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

fn decode_explicit_tokens(
    ctx: *mut c_void,
    tokens: &[ffi::llama_token],
    start_pos: ffi::llama_pos,
    logits: bool,
) -> Result<LlamaTokenBatch> {
    let mut batch = LlamaTokenBatch::new(tokens, start_pos, logits)?;
    let ffi_batch = batch.as_ffi();
    let code = unsafe { ffi::llama_decode(ctx, ffi_batch) };
    ensure!(code == 0, "llama.cpp decode failed with code {code}");
    Ok(batch)
}

fn sample_verified_tokens(
    sampler: *mut c_void,
    ctx: *mut c_void,
    draft: &[ffi::llama_token],
) -> Vec<ffi::llama_token> {
    let mut accepted = Vec::with_capacity(draft.len() + 1);
    for row in 0..=draft.len() {
        let token = unsafe { ffi::llama_sampler_sample(sampler, ctx, row as i32) };
        if row == 0 {
            accepted.push(token);
            continue;
        }
        if token != draft[row - 1] {
            break;
        }
        accepted.push(token);
    }
    accepted
}

fn rollback_rejected_mtp_tokens(
    ctx: *mut c_void,
    n_past: ffi::llama_pos,
    drafted: usize,
    accepted: usize,
) -> Result<()> {
    if accepted >= drafted {
        return Ok(());
    }

    let rollback_from = n_past
        .checked_add(1)
        .and_then(|pos| pos.checked_add(i32::try_from(accepted).ok()?))
        .context("llama.cpp MTP rollback position overflow")?;
    let removed =
        unsafe { ffi::llama_memory_seq_rm(ffi::llama_get_memory(ctx), 0, rollback_from, -1) };
    ensure!(
        removed,
        "llama.cpp failed to rollback rejected MTP draft tokens"
    );
    Ok(())
}

fn flush_mtp_pending_token(
    ctx: *mut c_void,
    mtp: &mut MtpState,
    token_history: &mut Vec<ffi::llama_token>,
    token: ffi::llama_token,
) -> Result<()> {
    ensure!(
        context_remaining(ctx) >= 1,
        "llama.cpp context has no room to flush final MTP token"
    );
    let mut batch =
        decode_explicit_tokens(ctx, &[token], history_len_as_pos(token_history)?, true)?;
    mtp.process(&mut batch)
        .context("failed to process final MTP token batch")?;
    token_history.push(token);
    Ok(())
}

fn decode_prompt_tokens(
    ctx: *mut c_void,
    tokens: &mut [ffi::llama_token],
    batch_size: usize,
    log: &mut String,
) -> Result<()> {
    let chunk_count = prefill_chunk_count(tokens.len(), batch_size)?;
    log.push_str("prompt_tokens = ");
    log.push_str(&tokens.len().to_string());
    log.push('\n');
    log.push_str("prefill_batch_size = ");
    log.push_str(&batch_size.to_string());
    log.push('\n');
    log.push_str("prefill_chunks = ");
    log.push_str(&chunk_count.to_string());
    log.push('\n');

    for (chunk_index, chunk) in tokens.chunks_mut(batch_size).enumerate() {
        decode_tokens(ctx, chunk).with_context(|| {
            format!(
                "failed to decode prompt chunk {}/{}",
                chunk_index + 1,
                chunk_count
            )
        })?;
    }
    Ok(())
}

fn prefill_chunk_count(token_count: usize, batch_size: usize) -> Result<usize> {
    ensure!(
        batch_size > 0,
        "llama.cpp prefill batch size cannot be zero"
    );
    Ok(if token_count == 0 {
        0
    } else {
        ((token_count - 1) / batch_size) + 1
    })
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

pub(super) fn trim_generated_continuation(content: &mut String) -> bool {
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

fn disable_qwen_thinking(prompt: &mut String) -> Option<&'static str> {
    const THINKING_PREFILL: &str = "<think>\n";
    const ASSISTANT_HEADER: &str = "<|im_start|>assistant\n";
    if prompt.ends_with(THINKING_PREFILL) {
        let truncate_to = prompt.len() - THINKING_PREFILL.len();
        prompt.truncate(truncate_to);
        prompt.push_str(DISABLED_THINKING_PREFILL);
        return Some(DISABLED_THINKING_PREFILL);
    }
    if prompt.ends_with(ASSISTANT_HEADER) {
        prompt.push_str(DISABLED_THINKING_PREFILL);
        return Some(DISABLED_THINKING_PREFILL);
    }
    None
}

fn is_context_empty(ctx: *mut c_void) -> bool {
    (unsafe { ffi::llama_memory_seq_pos_max(ffi::llama_get_memory(ctx), 0) }) == -1
}

fn context_next_pos(ctx: *mut c_void) -> ffi::llama_pos {
    (unsafe { ffi::llama_memory_seq_pos_max(ffi::llama_get_memory(ctx), 0) }) + 1
}

fn context_remaining(ctx: *mut c_void) -> ffi::llama_pos {
    (unsafe { ffi::llama_n_ctx(ctx) } as i32).saturating_sub(context_next_pos(ctx))
}

fn has_context_space(ctx: *mut c_void, requested_tokens: i32) -> bool {
    let used = unsafe { ffi::llama_memory_seq_pos_max(ffi::llama_get_memory(ctx), 0) } + 1;
    used.saturating_add(requested_tokens) < unsafe { ffi::llama_n_ctx(ctx) } as i32
}

fn history_len_as_pos(history: &[ffi::llama_token]) -> Result<ffi::llama_pos> {
    i32::try_from(history.len()).context("llama.cpp token history exceeds i32")
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

#[cfg(unix)]
fn path_cstring(path: &std::path::Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

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
    fn stop_marker_truncates_generated_user_continuation() {
        let mut content = "hello\n\nUser:\nnext".to_string();

        assert!(trim_generated_continuation(&mut content));
        assert_eq!(content, "hello\n");
    }

    #[test]
    fn stop_marker_truncates_generated_assistant_continuation() {
        let mut content = "hello\nAssistant:\nnext".to_string();

        assert!(trim_generated_continuation(&mut content));
        assert_eq!(content, "hello");
    }

    #[test]
    fn stop_marker_truncates_generated_tool_continuation() {
        let mut content = "hello\nTool:\nnext".to_string();

        assert!(trim_generated_continuation(&mut content));
        assert_eq!(content, "hello");
    }

    #[test]
    fn disables_qwen_thinking_prefill() {
        let mut prompt =
            "<|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\n<think>\n".to_string();

        assert_eq!(
            disable_qwen_thinking(&mut prompt),
            Some(DISABLED_THINKING_PREFILL)
        );
        assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
    }

    #[test]
    fn disables_qwen_thinking_after_plain_assistant_header() {
        let mut prompt = "<|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\n".to_string();

        assert_eq!(
            disable_qwen_thinking(&mut prompt),
            Some(DISABLED_THINKING_PREFILL)
        );
        assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
    }

    #[test]
    fn prepared_chat_messages_use_llama_roles() {
        let rendered = RenderedModelRequest {
            turn_id: "turn".to_string(),
            request_index: 0,
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
            messages: vec![
                RenderedMessage {
                    role: RenderedMessageRole::System,
                    content: "demo system".to_string(),
                    name: None,
                    tool_calls: Vec::new(),
                },
                RenderedMessage {
                    role: RenderedMessageRole::User,
                    content: "hello".to_string(),
                    name: None,
                    tool_calls: Vec::new(),
                },
            ],
            tools: vec![RenderedTool {
                name: "unused".to_string(),
                description: String::new(),
                required_arguments: Vec::new(),
            }],
        };

        let prepared = PreparedChatMessages::new(&rendered.messages).unwrap();

        assert_eq!(prepared.messages.len(), 2);
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[0].role) }
                .to_str()
                .unwrap(),
            "system"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[0].content) }
                .to_str()
                .unwrap(),
            "demo system"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[1].role) }
                .to_str()
                .unwrap(),
            "user"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[1].content) }
                .to_str()
                .unwrap(),
            "hello"
        );
    }

    #[test]
    fn prefill_chunk_count_splits_prompt_by_batch_size() {
        assert_eq!(prefill_chunk_count(0, 1024).unwrap(), 0);
        assert_eq!(prefill_chunk_count(1, 1024).unwrap(), 1);
        assert_eq!(prefill_chunk_count(1024, 1024).unwrap(), 1);
        assert_eq!(prefill_chunk_count(1025, 1024).unwrap(), 2);
        assert_eq!(prefill_chunk_count(4096, 1024).unwrap(), 4);
    }

    #[test]
    fn prefill_chunk_count_rejects_zero_batch_size() {
        let err = prefill_chunk_count(1, 0).unwrap_err();

        assert!(
            format!("{err:#}").contains("llama.cpp prefill batch size cannot be zero"),
            "unexpected error: {err:#}"
        );
    }
}
