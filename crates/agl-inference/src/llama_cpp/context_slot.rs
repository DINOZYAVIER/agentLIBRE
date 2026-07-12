use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::ptr;
use std::rc::Rc;

use agl_actions::{ModelAction, ToolCall, ToolJsonRepair, parse_model_action};
use agl_config::{
    InferenceRuntimeConfig, KvCacheType, LocalInferenceConfig, RuntimeSwitch, ToolCallFormat,
};
use agl_content::Content;
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};
use anyhow::{Context, Result, bail, ensure};

use crate::InferenceFinishReason;

use super::ffi;
use super::generation::{
    LlamaCppGenerationCancelled, LlamaCppGenerationControl, LlamaCppGenerationOutput,
};
use super::model::LlamaCppModel;

const DISABLED_THINKING_PREFILL: &str = "<think>\n\n</think>\n\n";
const QWEN_ASSISTANT_HEADER: &str = "<|im_start|>assistant\n";
const QWEN_DISABLED_THINKING_PREFIX: &str = "<|im_start|>assistant\n<think>\n\n</think>\n\n";
const GEMMA_MODEL_HEADER: &str = "<|turn>model\n";
const GEMMA_THOUGHT_CHANNEL_PREFIX: &str = "<|channel>thought\n<channel|>";
const GEMMA_THOUGHT_PREFIX: &str = "<|turn>model\n<|channel>thought\n<channel|>";
const AGL_LLAMA_MTP_OK: i32 = 0;

/// Mutable, per-conversation llama.cpp state owned by the native worker.
pub struct LlamaCppContextSlot {
    runtime: InferenceRuntimeConfig,
    // Declaration order keeps sampler/speculative/draft context ahead of the
    // target context in Rust's field drop order.
    sampler: Sampler,
    mtp: Option<MtpState>,
    context: ContextHandle,
    cache: ContextCache,
    prefill_batch_size: usize,
    _thread_bound: PhantomData<Rc<()>>,
}

#[derive(Default)]
struct ContextCache {
    rendered_message_history_len: usize,
    messages: Vec<RenderedMessage>,
    token_history: Vec<ffi::llama_token>,
    formatted_history: String,
    normalized_history: String,
    cache_matches_transcript: bool,
}

impl LlamaCppContextSlot {
    pub(crate) fn new(
        model: &LlamaCppModel,
        config: &LocalInferenceConfig,
        log: &mut String,
    ) -> Result<Self> {
        Self::from_runtime(model, &config.runtime, log)
    }

    fn from_runtime(
        model: &LlamaCppModel,
        runtime: &InferenceRuntimeConfig,
        log: &mut String,
    ) -> Result<Self> {
        let mut context_params = unsafe { ffi::llama_context_default_params() };
        context_params.n_ctx = runtime.context_tokens;
        let prefill_batch_size = runtime.batch_size.unwrap_or(runtime.context_tokens);
        context_params.n_batch = prefill_batch_size;
        if let Some(ubatch_size) = runtime.ubatch_size {
            context_params.n_ubatch = ubatch_size;
        }
        context_params.n_threads =
            i32::try_from(runtime.threads).context("llama.cpp threads exceeds i32")?;
        context_params.n_threads_batch = context_params.n_threads;
        context_params.flash_attn_type = map_flash_attention(runtime.flash_attention);
        if let Some(kv_unified) = runtime.kv_unified {
            context_params.kv_unified = kv_unified;
        }
        if let Some(cache_type) = runtime.cache_type_k {
            context_params.type_k = map_cache_type(cache_type);
        }
        if let Some(cache_type) = runtime.cache_type_v {
            context_params.type_v = map_cache_type(cache_type);
        }
        if runtime.mtp.enabled {
            context_params.n_outputs_max = runtime.mtp.draft_tokens.saturating_add(1);
            context_params.n_rs_seq = runtime.mtp.draft_tokens;
        }

        let context = ContextHandle::new(model.main_ptr(), context_params)
            .context("failed to create llama.cpp context")?;
        log.push_str("n_ctx = ");
        log.push_str(&unsafe { ffi::llama_n_ctx(context.as_ptr()) }.to_string());
        log.push('\n');

        let sampler = Sampler::greedy().context("failed to create llama.cpp sampler")?;
        let prefill_batch_size =
            usize::try_from(prefill_batch_size).context("llama.cpp n_batch exceeds usize")?;
        ensure!(
            prefill_batch_size > 0,
            "llama.cpp n_batch must be greater than zero"
        );
        let mtp = if runtime.mtp.enabled {
            Some(
                MtpState::new(model, runtime, context.as_ptr(), prefill_batch_size, log)
                    .context("failed to initialize llama.cpp MTP state")?,
            )
        } else {
            None
        };

        Ok(Self {
            runtime: runtime.clone(),
            sampler,
            mtp,
            context,
            cache: ContextCache {
                cache_matches_transcript: true,
                ..ContextCache::default()
            },
            prefill_batch_size,
            _thread_bound: PhantomData,
        })
    }

    pub(crate) fn matches_config(&self, config: &LocalInferenceConfig) -> bool {
        self.runtime == config.runtime
    }

    pub(crate) fn reset_cache(
        &mut self,
        model: &LlamaCppModel,
        config: &LocalInferenceConfig,
        log: &mut String,
    ) -> Result<()> {
        self.rebuild(model, &config.runtime, log)
    }

    pub(crate) fn clear_cache(&mut self, model: &LlamaCppModel, log: &mut String) -> Result<()> {
        let runtime = self.runtime.clone();
        self.rebuild(model, &runtime, log)
    }

    fn rebuild(
        &mut self,
        model: &LlamaCppModel,
        runtime: &InferenceRuntimeConfig,
        log: &mut String,
    ) -> Result<()> {
        let replacement = Self::from_runtime(model, runtime, log)?;
        *self = replacement;
        log.push_str("llama_cpp_context_cache = rebuilt\n");
        Ok(())
    }

    pub(crate) fn rendered_append_error(
        &self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
    ) -> Option<String> {
        self.render_prompt_append(model, rendered)
            .err()
            .map(|error| format!("{error:#}"))
    }

    fn render_prompt_append(
        &self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
    ) -> Result<PromptTemplateAppend> {
        ensure!(
            self.cache.cache_matches_transcript,
            "cached generation contains a trimmed continuation"
        );
        if !rendered_history_is_prefix(
            &self.cache.messages,
            &rendered.messages,
            self.cache.rendered_message_history_len,
        ) {
            bail!("rendered_message_history_changed");
        }

        let mut messages = self.cache.messages.clone();
        messages.extend(
            rendered.messages[self.cache.rendered_message_history_len..]
                .iter()
                .cloned(),
        );
        let mut formatted = apply_chat_template_messages(
            model.main_ptr().cast_const(),
            &messages,
            rendered.tool_call_format,
            true,
        )
        .context("failed to render llama.cpp chat template")?;
        let injected_assistant_prefix = disable_qwen_thinking(&mut formatted);
        let assistant_context_prefix = if formatted.ends_with(DISABLED_THINKING_PREFILL) {
            DISABLED_THINKING_PREFILL.to_string()
        } else {
            injected_assistant_prefix.unwrap_or_default().to_string()
        };
        let normalized_formatted = normalize_assistant_context(&formatted);
        let common_prefix_bytes =
            common_prefix_len(&self.cache.normalized_history, &normalized_formatted);
        if common_prefix_bytes != self.cache.normalized_history.len() {
            bail!(
                "chat template rewrote cached history: exact_history_bytes={}, normalized_history_bytes={}, incoming_bytes={}, common_prefix_bytes={common_prefix_bytes}, history_tail={:?}, incoming_tail={:?}",
                self.cache.formatted_history.len(),
                self.cache.normalized_history.len(),
                formatted.len(),
                mismatch_excerpt(&self.cache.normalized_history, common_prefix_bytes),
                mismatch_excerpt(&normalized_formatted, common_prefix_bytes),
            );
        }
        let history_prefix_len =
            source_index_after_normalized_prefix(&formatted, self.cache.normalized_history.len())
                .context("llama.cpp normalized history is not a source prompt boundary")?;
        let prompt = formatted[history_prefix_len..].to_string();
        ensure!(!prompt.is_empty(), "llama.cpp prompt append is empty");

        let formatted_prompt = format!("{}{prompt}", self.cache.formatted_history);
        let formatted_tokens = tokenize(model.vocab(), &formatted_prompt, true)?;
        let common_prefix_tokens = self
            .cache
            .token_history
            .iter()
            .zip(&formatted_tokens)
            .take_while(|(recorded, incoming)| recorded == incoming)
            .count();
        ensure!(
            common_prefix_tokens == self.cache.token_history.len(),
            "chat template retokenized cached prompt: cached_tokens={}, incoming_tokens={}, common_prefix_tokens={common_prefix_tokens}",
            self.cache.token_history.len(),
            formatted_tokens.len(),
        );
        let tokens = formatted_tokens[self.cache.token_history.len()..].to_vec();
        ensure!(!tokens.is_empty(), "llama.cpp prompt append has no tokens");

        Ok(PromptTemplateAppend {
            prompt,
            tokens,
            history: PreparedPromptHistory {
                assistant_context_prefix,
                formatted_prompt,
            },
            messages,
        })
    }

    pub(crate) fn generate(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        max_output_tokens: u32,
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
    ) -> Result<LlamaCppGenerationOutput> {
        control.ensure_running()?;
        let draft_context = self.mtp.as_ref().map(MtpState::draft_context_ptr);
        let abort_guard = control.install_abort_callback(self.context.as_ptr(), draft_context);
        let result = self.generate_inner(model, rendered, max_output_tokens, control, log);
        drop(abort_guard);

        if control.should_abort() {
            self.cache.cache_matches_transcript = false;
            return Err(LlamaCppGenerationCancelled.into());
        }
        if result.is_err() {
            self.cache.cache_matches_transcript = false;
        }
        result
    }

    pub(crate) fn generate_vision(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        images: &[&[u8]],
        max_output_tokens: u32,
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
    ) -> Result<LlamaCppGenerationOutput> {
        control.ensure_running()?;
        ensure!(
            self.mtp.is_none(),
            "llama.cpp vision cannot use speculative MTP"
        );
        let abort_guard = control.install_abort_callback(self.context.as_ptr(), None);
        let result =
            self.generate_vision_inner(model, rendered, images, max_output_tokens, control, log);
        drop(abort_guard);

        self.cache.cache_matches_transcript = false;
        if control.should_abort() {
            return Err(LlamaCppGenerationCancelled.into());
        }
        result
    }

    fn generate_inner(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        max_output_tokens: u32,
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
    ) -> Result<LlamaCppGenerationOutput> {
        let prepared = self.prepare_prompt_append(model, rendered, log)?;

        ensure!(
            !prepared.tokens.is_empty(),
            "llama.cpp prompt produced no tokens"
        );
        let prompt_token_count = i32::try_from(prepared.tokens.len())
            .context("llama.cpp prompt token count exceeds i32")?;
        ensure!(
            has_context_space(self.context.as_ptr(), prompt_token_count),
            "llama.cpp prompt exceeds remaining context"
        );
        if self.mtp.is_some() {
            return self.generate_with_mtp(
                model,
                rendered,
                prepared,
                max_output_tokens,
                control,
                log,
            );
        }
        let PreparedPrompt {
            tokens: mut prompt_tokens,
            messages: prompt_messages,
            history: prompt_history,
        } = prepared;
        let input_tokens = u64::try_from(prompt_tokens.len()).unwrap_or(u64::MAX);

        decode_prompt_tokens(
            self.context.as_ptr(),
            &mut prompt_tokens,
            self.prefill_batch_size,
            log,
        )
        .context("failed to decode prompt")?;
        self.cache.token_history.extend_from_slice(&prompt_tokens);

        self.generate_after_prefill(
            model,
            rendered,
            prompt_messages,
            prompt_history,
            input_tokens,
            max_output_tokens,
            control,
        )
    }

    fn generate_vision_inner(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        images: &[&[u8]],
        max_output_tokens: u32,
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
    ) -> Result<LlamaCppGenerationOutput> {
        ensure!(
            self.cache.rendered_message_history_len == 0 && self.cache.token_history.is_empty(),
            "llama.cpp vision requires a fresh context"
        );
        let prepared = self.prepare_prompt_append(model, rendered, log)?;
        let PreparedPrompt {
            messages: prompt_messages,
            history: prompt_history,
            ..
        } = prepared;
        let (positions, input_tokens) = model
            .eval_vision(
                self.context.as_ptr(),
                &prompt_history.formatted_prompt,
                images,
                self.prefill_batch_size,
            )
            .context("failed to encode multimodal prompt")?;
        log.push_str("multimodal_images = ");
        log.push_str(&images.len().to_string());
        log.push('\n');
        log.push_str("multimodal_prompt_positions = ");
        log.push_str(&positions.to_string());
        log.push('\n');
        log.push_str("multimodal_input_tokens = ");
        log.push_str(&input_tokens.to_string());
        log.push('\n');

        self.generate_after_prefill(
            model,
            rendered,
            prompt_messages,
            prompt_history,
            u64::try_from(input_tokens).unwrap_or(u64::MAX),
            max_output_tokens,
            control,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn generate_after_prefill(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        prompt_messages: Vec<RenderedMessage>,
        prompt_history: PreparedPromptHistory,
        input_tokens: u64,
        max_output_tokens: u32,
        control: &LlamaCppGenerationControl<'_>,
    ) -> Result<LlamaCppGenerationOutput> {
        let mut content = String::new();
        let mut decoded_content = String::new();
        let mut finish_reason = InferenceFinishReason::Length;
        let mut output_tokens = 0_u64;
        for _ in 0..max_output_tokens {
            control.ensure_running()?;
            if !has_context_space(self.context.as_ptr(), 1) {
                finish_reason = InferenceFinishReason::Length;
                break;
            }
            let token = unsafe {
                ffi::llama_sampler_sample(self.sampler.as_ptr(), self.context.as_ptr(), -1)
            };
            if unsafe { ffi::llama_vocab_is_eog(model.vocab(), token) } {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }

            let piece = token_to_piece(model.vocab(), token)?;
            let mut next_token = [token];
            decode_tokens(self.context.as_ptr(), &mut next_token)
                .context("failed to decode generated token")?;
            self.cache.token_history.push(token);
            output_tokens = output_tokens.saturating_add(1);
            decoded_content.push_str(&piece);
            content.push_str(&piece);
            strip_generated_assistant_prefix(&mut content);
            if isolated_tool_call(&content).is_some() {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }
            if trim_generated_continuation(&mut content) {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }
        }

        self.record_generated_assistant(
            rendered,
            prompt_messages,
            prompt_history,
            &decoded_content,
            &content,
        )?;

        Ok(LlamaCppGenerationOutput {
            content,
            finish_reason,
            input_tokens,
            output_tokens,
        })
    }

    fn generate_with_mtp(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        prepared: PreparedPrompt,
        max_output_tokens: u32,
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
    ) -> Result<LlamaCppGenerationOutput> {
        let Some(mut mtp) = self.mtp.take() else {
            bail!("llama.cpp MTP state is missing");
        };
        let result = self.generate_with_mtp_state(
            model,
            rendered,
            prepared,
            max_output_tokens,
            control,
            log,
            &mut mtp,
        );
        self.mtp = Some(mtp);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn generate_with_mtp_state(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        prepared: PreparedPrompt,
        max_output_tokens: u32,
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
        mtp: &mut MtpState,
    ) -> Result<LlamaCppGenerationOutput> {
        let PreparedPrompt {
            tokens: prompt_tokens,
            messages: prompt_messages,
            history: prompt_history,
        } = prepared;
        let input_tokens = u64::try_from(prompt_tokens.len()).unwrap_or(u64::MAX);
        log.push_str("mtp_generation_mode = draft-mtp\n");
        log.push_str("mtp_sequence_mode = seq0-temporary\n");

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
                self.cache.token_history.extend_from_slice(chunk);
                start_pos += i32::try_from(chunk.len()).context("MTP prompt chunk too large")?;
            }
        } else {
            log.push_str("prompt_tokens = 1\n");
            log.push_str("mtp_prefill_tokens = 0\n");
            log.push_str("prefill_chunks = 0\n");
        }

        mtp.begin(&self.cache.token_history)
            .context("failed to begin MTP speculation")?;

        let mut content = String::new();
        let mut decoded_content = String::new();
        let mut finish_reason = InferenceFinishReason::Length;
        let mut emitted = 0_u32;
        let mut pending_needs_flush = false;
        let mut stopped_on_eog = false;
        let mut tool_call_completed = false;

        while emitted < max_output_tokens {
            control.ensure_running()?;
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
                    history_len_as_pos(&self.cache.token_history)?,
                    id_last,
                    &self.cache.token_history,
                )
                .context("failed to draft MTP tokens")?
            } else {
                Vec::new()
            };

            let n_past = history_len_as_pos(&self.cache.token_history)?;
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

            self.cache.token_history.push(id_last);
            self.cache
                .token_history
                .extend(accepted.iter().take(n_accepted).copied());
            pending_needs_flush = true;

            for token in &accepted {
                id_last = *token;
                if unsafe { ffi::llama_vocab_is_eog(model.vocab(), id_last) } {
                    finish_reason = InferenceFinishReason::Stop;
                    stopped_on_eog = true;
                    pending_needs_flush = false;
                    break;
                }

                let piece = token_to_piece(model.vocab(), id_last)?;
                decoded_content.push_str(&piece);
                emitted += 1;
                if tool_call_completed {
                    continue;
                }
                content.push_str(&piece);
                strip_generated_assistant_prefix(&mut content);
                if isolated_tool_call(&content).is_some() {
                    finish_reason = InferenceFinishReason::Stop;
                    tool_call_completed = true;
                    continue;
                }
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
                mtp,
                &mut self.cache.token_history,
                id_last,
            )
            .context("failed to flush final MTP token")?;
        }

        mtp.write_stats_log(log);

        self.record_generated_assistant(
            rendered,
            prompt_messages,
            prompt_history,
            &decoded_content,
            &content,
        )?;

        Ok(LlamaCppGenerationOutput {
            content,
            finish_reason,
            input_tokens,
            output_tokens: u64::from(emitted),
        })
    }

    fn prepare_prompt_append(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        log: &mut String,
    ) -> Result<PreparedPrompt> {
        if rendered.messages.len() < self.cache.rendered_message_history_len {
            bail!(
                "llama.cpp session cannot append {} rendered messages after {} were recorded",
                rendered.messages.len(),
                self.cache.rendered_message_history_len
            );
        }
        let PromptTemplateAppend {
            prompt,
            tokens: prompt_tokens,
            history,
            messages,
        } = self.render_prompt_append(model, rendered)?;
        if !history.assistant_context_prefix.is_empty() {
            log.push_str("thinking_prefill = disabled\n");
        }

        log.push_str("rendered_message_history_len = ");
        log.push_str(&self.cache.rendered_message_history_len.to_string());
        log.push('\n');
        log.push_str("cached_prompt_tokens = ");
        log.push_str(&self.cache.token_history.len().to_string());
        log.push('\n');
        log.push_str("prompt_append_tokens = ");
        log.push_str(&prompt_tokens.len().to_string());
        log.push('\n');
        log.push_str("llama_cpp_prompt_append:\n");
        log.push_str(&prompt);
        if !prompt.ends_with('\n') {
            log.push('\n');
        }

        Ok(PreparedPrompt {
            tokens: prompt_tokens,
            messages,
            history,
        })
    }

    fn record_generated_assistant(
        &mut self,
        rendered: &RenderedModelRequest,
        mut messages: Vec<RenderedMessage>,
        prompt_history: PreparedPromptHistory,
        decoded_content: &str,
        content: &str,
    ) -> Result<()> {
        let PreparedPromptHistory {
            assistant_context_prefix,
            formatted_prompt,
        } = prompt_history;
        messages.push(RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: Some(Content::text(format!(
                "{assistant_context_prefix}{content}"
            ))?),
            name: None,
            tool_calls: Vec::new(),
        });
        self.cache.messages = messages;
        self.cache.formatted_history = format!("{formatted_prompt}{decoded_content}");
        self.cache.normalized_history = normalize_assistant_context(&self.cache.formatted_history);
        self.cache.cache_matches_transcript = decoded_content == content;
        self.cache.rendered_message_history_len = rendered.messages.len() + 1;
        Ok(())
    }
}

pub(super) fn rendered_history_is_prefix(
    recorded: &[RenderedMessage],
    incoming: &[RenderedMessage],
    history_len: usize,
) -> bool {
    incoming.len() >= history_len
        && recorded.len() >= history_len
        && recorded[..history_len]
            .iter()
            .zip(&incoming[..history_len])
            .all(|(recorded, incoming)| rendered_history_message_matches(recorded, incoming))
}

fn rendered_history_message_matches(
    recorded: &RenderedMessage,
    incoming: &RenderedMessage,
) -> bool {
    if recorded.role != incoming.role {
        return false;
    }
    let recorded_role = recorded.role;
    let (Ok(mut recorded_content), Ok(incoming_content)) = (
        rendered_message_content(recorded),
        rendered_message_content(incoming),
    ) else {
        return false;
    };
    if recorded_content == incoming_content {
        return true;
    }
    if recorded_role != RenderedMessageRole::Assistant {
        return false;
    }
    if let Some(content) = recorded_content.strip_prefix(DISABLED_THINKING_PREFILL) {
        recorded_content = content.to_string();
        if recorded_content == incoming_content {
            return true;
        }
    }

    matches!(
        (
            isolated_tool_call(&recorded_content),
            isolated_tool_call(&incoming_content),
        ),
        (Some(recorded), Some(incoming)) if recorded == incoming
    )
}

fn isolated_tool_call(content: &str) -> Option<ToolCall> {
    let content = content.trim();
    let isolated = isolated_block(content, "<tool_call>", "</tool_call>")
        || isolated_block(content, "<|tool_call>", "<tool_call|>");
    if !isolated {
        return None;
    }
    match parse_model_action(content) {
        ModelAction::ToolCall(tool_call) => Some(tool_call),
        ModelAction::MalformedToolCall(malformed) => match malformed.repair {
            Some(ToolJsonRepair::Succeeded { tool_call, .. }) => Some(tool_call),
            Some(ToolJsonRepair::Failed { .. }) | None => None,
        },
        ModelAction::Answer(_) => None,
    }
}

fn isolated_block(content: &str, open: &str, close: &str) -> bool {
    content
        .strip_prefix(open)
        .and_then(|content| content.strip_suffix(close))
        .is_some_and(|content| !content.contains(open) && !content.contains(close))
}

fn common_prefix_len(recorded: &str, incoming: &str) -> usize {
    recorded
        .bytes()
        .zip(incoming.bytes())
        .take_while(|(recorded, incoming)| recorded == incoming)
        .count()
}

fn normalize_assistant_context(value: &str) -> String {
    value
        .replace(QWEN_DISABLED_THINKING_PREFIX, QWEN_ASSISTANT_HEADER)
        .replace(GEMMA_THOUGHT_PREFIX, "")
        .replace(GEMMA_THOUGHT_CHANNEL_PREFIX, "")
        .replace(GEMMA_MODEL_HEADER, "")
}

fn strip_generated_assistant_prefix(content: &mut String) {
    for prefix in [
        GEMMA_THOUGHT_PREFIX,
        GEMMA_THOUGHT_CHANNEL_PREFIX,
        GEMMA_MODEL_HEADER,
    ] {
        if content.starts_with(prefix) {
            content.drain(..prefix.len());
        }
    }
}

fn source_index_after_normalized_prefix(
    source: &str,
    normalized_prefix_len: usize,
) -> Option<usize> {
    let mut source_index = 0;
    let mut normalized_index = 0;
    while normalized_index < normalized_prefix_len {
        let remaining = &source[source_index..];
        if let Some((source_len, normalized_len)) = assistant_context_rewrite(remaining) {
            let needed = normalized_prefix_len - normalized_index;
            if needed < normalized_len {
                return Some(source_index + needed);
            }
            normalized_index += normalized_len;
            source_index += source_len;
            continue;
        }
        let char_len = remaining.chars().next()?.len_utf8();
        if normalized_index + char_len > normalized_prefix_len {
            return None;
        }
        normalized_index += char_len;
        source_index += char_len;
    }
    Some(source_index)
}

fn assistant_context_rewrite(value: &str) -> Option<(usize, usize)> {
    if value.starts_with(QWEN_DISABLED_THINKING_PREFIX) {
        Some((
            QWEN_DISABLED_THINKING_PREFIX.len(),
            QWEN_ASSISTANT_HEADER.len(),
        ))
    } else if value.starts_with(GEMMA_THOUGHT_PREFIX) {
        Some((GEMMA_THOUGHT_PREFIX.len(), 0))
    } else if value.starts_with(GEMMA_THOUGHT_CHANNEL_PREFIX) {
        Some((GEMMA_THOUGHT_CHANNEL_PREFIX.len(), 0))
    } else if value.starts_with(GEMMA_MODEL_HEADER) {
        Some((GEMMA_MODEL_HEADER.len(), 0))
    } else {
        None
    }
}

fn mismatch_excerpt(value: &str, offset: usize) -> String {
    let bytes = &value.as_bytes()[offset.min(value.len())..];
    String::from_utf8_lossy(&bytes[..bytes.len().min(160)]).into_owned()
}

struct PreparedPrompt {
    tokens: Vec<ffi::llama_token>,
    messages: Vec<RenderedMessage>,
    history: PreparedPromptHistory,
}

struct PreparedPromptHistory {
    assistant_context_prefix: String,
    formatted_prompt: String,
}

struct PromptTemplateAppend {
    prompt: String,
    tokens: Vec<ffi::llama_token>,
    history: PreparedPromptHistory,
    messages: Vec<RenderedMessage>,
}

struct MtpState {
    speculative: MtpSpeculative,
    draft_context: ContextHandle,
    draft_tokens: usize,
}

impl MtpState {
    fn new(
        model: &LlamaCppModel,
        runtime: &InferenceRuntimeConfig,
        target_context: *mut c_void,
        prefill_batch_size: usize,
        log: &mut String,
    ) -> Result<Self> {
        ensure!(
            runtime.mtp.draft_tokens > 0,
            "runtime.mtp draft_tokens must be greater than zero"
        );
        let draft_tokens = usize::try_from(runtime.mtp.draft_tokens)
            .context("runtime.mtp draft_tokens exceeds usize")?;
        let draft_model = model
            .draft_ptr()
            .context("runtime.mtp enabled model has no draft weights")?;

        let mut context_params = unsafe { ffi::llama_context_default_params() };
        context_params.n_ctx = runtime.context_tokens;
        context_params.n_batch =
            u32::try_from(prefill_batch_size).context("llama.cpp MTP n_batch exceeds u32")?;
        if let Some(ubatch_size) = runtime.ubatch_size {
            context_params.n_ubatch = ubatch_size;
        }
        context_params.n_threads =
            i32::try_from(runtime.threads).context("llama.cpp MTP threads exceeds i32")?;
        context_params.n_threads_batch = context_params.n_threads;
        context_params.flash_attn_type = map_flash_attention(runtime.flash_attention);
        context_params.ctx_type = ffi::LLAMA_CONTEXT_TYPE_MTP;
        context_params.n_rs_seq = 0;
        context_params.n_outputs_max = 1;
        context_params.ctx_other = target_context;
        if let Some(kv_unified) = runtime.kv_unified {
            context_params.kv_unified = kv_unified;
        }
        if let Some(cache_type) = runtime.mtp.cache_type_k.or(runtime.cache_type_k) {
            context_params.type_k = map_cache_type(cache_type);
        }
        if let Some(cache_type) = runtime.mtp.cache_type_v.or(runtime.cache_type_v) {
            context_params.type_v = map_cache_type(cache_type);
        }

        let draft_context = ContextHandle::new(draft_model, context_params)
            .context("failed to create llama.cpp MTP draft context")?;
        let speculative = MtpSpeculative::new(
            target_context,
            draft_context.as_ptr(),
            i32::try_from(draft_tokens).context("runtime.mtp draft_tokens exceeds i32")?,
            runtime.mtp.p_min.as_f32(),
        )?;

        log.push_str("mtp_runtime_state = active\n");
        log.push_str("mtp_sequence_mode = seq0-temporary\n");

        Ok(Self {
            speculative,
            draft_context,
            draft_tokens,
        })
    }

    fn draft_context_ptr(&self) -> *mut c_void {
        self.draft_context.as_ptr()
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
    tool_call_format: ToolCallFormat,
    add_assistant: bool,
) -> Result<String> {
    let prepared = PreparedChatMessages::new(messages, tool_call_format)?;
    apply_common_chat_template(model, &prepared, add_assistant)
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
    _names: Vec<CString>,
    _tool_call_names: Vec<CString>,
    _tool_call_arguments: Vec<CString>,
    _tool_calls: Vec<Vec<ffi::agl_llama_chat_tool_call>>,
    messages: Vec<ffi::agl_llama_chat_message>,
}

impl PreparedChatMessages {
    fn new(messages: &[RenderedMessage], tool_call_format: ToolCallFormat) -> Result<Self> {
        let render_structured_tool_fields = tool_call_format == ToolCallFormat::GemmaFunctionCall;
        let mut roles = Vec::with_capacity(messages.len());
        let mut contents = Vec::with_capacity(messages.len());
        let mut names = Vec::new();
        let mut tool_call_names = Vec::new();
        let mut tool_call_arguments = Vec::new();
        let mut tool_calls = Vec::with_capacity(messages.len());
        let mut ffi_messages = Vec::with_capacity(messages.len());

        for message in messages {
            let role = CString::new(match message.role {
                RenderedMessageRole::System => "system",
                RenderedMessageRole::User => "user",
                RenderedMessageRole::Assistant => "assistant",
                RenderedMessageRole::Tool => "tool",
            })?;
            let structured_tool_calls = render_structured_tool_fields
                && message.role == RenderedMessageRole::Assistant
                && !message.tool_calls.is_empty();
            let content = if structured_tool_calls {
                CString::new(String::new())?
            } else {
                CString::new(rendered_message_content(message)?)?
            };
            let name = if render_structured_tool_fields && message.role == RenderedMessageRole::Tool
            {
                match &message.name {
                    Some(name) => {
                        names.push(CString::new(name.as_str())?);
                        names.last().map_or(ptr::null(), |name| name.as_ptr())
                    }
                    None => ptr::null(),
                }
            } else {
                ptr::null()
            };
            let mut ffi_tool_calls = Vec::new();
            if structured_tool_calls {
                ffi_tool_calls.reserve(message.tool_calls.len());
                for tool_call in &message.tool_calls {
                    tool_call_names.push(CString::new(tool_call.name.as_str())?);
                    tool_call_arguments
                        .push(CString::new(serde_json::to_string(&tool_call.arguments)?)?);
                    ffi_tool_calls.push(ffi::agl_llama_chat_tool_call {
                        name: tool_call_names
                            .last()
                            .map_or(ptr::null(), |name| name.as_ptr()),
                        arguments: tool_call_arguments
                            .last()
                            .map_or(ptr::null(), |arguments| arguments.as_ptr()),
                        id: ptr::null(),
                    });
                }
            }
            let n_tool_calls = ffi_tool_calls.len();
            tool_calls.push(ffi_tool_calls);
            let tool_calls_ptr = tool_calls
                .last()
                .filter(|tool_calls| !tool_calls.is_empty())
                .map_or(ptr::null(), |tool_calls| tool_calls.as_ptr());
            ffi_messages.push(ffi::agl_llama_chat_message {
                role: role.as_ptr(),
                content: content.as_ptr(),
                name,
                tool_calls: tool_calls_ptr,
                n_tool_calls,
            });
            roles.push(role);
            contents.push(content);
        }

        Ok(Self {
            _roles: roles,
            _contents: contents,
            _names: names,
            _tool_call_names: tool_call_names,
            _tool_call_arguments: tool_call_arguments,
            _tool_calls: tool_calls,
            messages: ffi_messages,
        })
    }
}

pub(crate) fn rendered_message_content(message: &RenderedMessage) -> Result<String> {
    let mut content = match &message.content {
        Some(content) => content.text_only().context(
            "unsupported_content: llama.cpp text rendering cannot consume artifact references",
        )?,
        None => String::new(),
    };
    if content.is_empty() {
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
    if prompt.ends_with(THINKING_PREFILL) {
        let truncate_to = prompt.len() - THINKING_PREFILL.len();
        prompt.truncate(truncate_to);
        prompt.push_str(DISABLED_THINKING_PREFILL);
        return Some(DISABLED_THINKING_PREFILL);
    }
    if prompt.ends_with(QWEN_ASSISTANT_HEADER) {
        prompt.push_str(DISABLED_THINKING_PREFILL);
        return Some(DISABLED_THINKING_PREFILL);
    }
    None
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

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use agl_config::{ModelDialect, ToolCallFormat};
    use agl_oven::{RenderedTool, RenderedToolCall};
    use serde_json::json;

    use super::*;

    fn text(value: impl Into<String>) -> Option<Content> {
        Some(Content::text(value).unwrap())
    }

    #[test]
    fn context_caches_keep_conversation_state_isolated() {
        let mut first = ContextCache {
            cache_matches_transcript: true,
            ..ContextCache::default()
        };
        let second = ContextCache {
            cache_matches_transcript: true,
            ..ContextCache::default()
        };

        first.messages.push(RenderedMessage {
            role: RenderedMessageRole::User,
            content: text("first conversation"),
            name: None,
            tool_calls: Vec::new(),
        });
        first.token_history.extend([11, 12, 13]);
        first.formatted_history.push_str("first conversation");
        first.rendered_message_history_len = 1;

        assert!(second.messages.is_empty());
        assert!(second.token_history.is_empty());
        assert!(second.formatted_history.is_empty());
        assert_eq!(second.rendered_message_history_len, 0);
        assert!(second.cache_matches_transcript);
    }

    #[test]
    fn rendered_message_content_serializes_tool_calls_without_text() {
        let message = RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: None,
            name: None,
            tool_calls: vec![RenderedToolCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}),
            }],
        };

        let content = rendered_message_content(&message).unwrap();

        assert!(content.contains("\"name\":\"read_file\""));
        assert!(content.contains("\"path\":\"README.md\""));
    }

    #[test]
    fn rendered_message_content_keeps_canonical_text_when_tool_calls_are_structured() {
        let message = RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: text("<|tool_call>call:screen.capture{}<tool_call|>"),
            name: Some("screen.capture".to_string()),
            tool_calls: vec![RenderedToolCall {
                name: "screen.capture".to_string(),
                arguments: json!({}),
            }],
        };

        let content = rendered_message_content(&message).unwrap();

        assert_eq!(content, "<|tool_call>call:screen.capture{}<tool_call|>");
    }

    #[test]
    fn rendered_history_matches_only_isolated_semantic_tool_calls() {
        let recorded = RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: text(format!(
                "{DISABLED_THINKING_PREFILL}{}",
                r#"<tool_call>{"name":"fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>"#,
            )),
            name: None,
            tool_calls: Vec::new(),
        };
        let canonical = RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: text(
                r#"<tool_call>{"arguments":{"limit_lines":20,"path":"facts.txt"},"name":"fs.read"}</tool_call>"#,
            ),
            name: Some("fs.read".to_string()),
            tool_calls: Vec::new(),
        };
        let mut changed = canonical.clone();
        changed.content = text(
            r#"<tool_call>{"arguments":{"limit_lines":20,"path":"other.txt"},"name":"fs.read"}</tool_call>"#,
        );
        let mut with_prose = canonical.clone();
        with_prose.content = text(format!(
            "calling now\n{}",
            rendered_message_content(&canonical).unwrap()
        ));
        let mut user_call = canonical.clone();
        user_call.role = RenderedMessageRole::User;
        let mut user_call_reordered = user_call.clone();
        user_call_reordered.content = text(
            r#"<tool_call>{"name":"fs.read","arguments":{"path":"facts.txt","limit_lines":20}}</tool_call>"#,
        );
        let repaired = RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: text(
                r#"<tool_call>"{\"name\":\"fs.read\",\"arguments\":{\"path\":\"facts.txt\",\"limit_lines\":20}}"</tool_call>"#,
            ),
            name: None,
            tool_calls: Vec::new(),
        };
        let mut same_prefill = canonical.clone();
        same_prefill.content = text(format!(
            "{DISABLED_THINKING_PREFILL}{}",
            rendered_message_content(&canonical).unwrap()
        ));

        assert!(rendered_history_is_prefix(
            std::slice::from_ref(&recorded),
            std::slice::from_ref(&canonical),
            1,
        ));
        assert!(!rendered_history_is_prefix(
            std::slice::from_ref(&recorded),
            std::slice::from_ref(&changed),
            1,
        ));
        assert!(rendered_history_is_prefix(
            std::slice::from_ref(&same_prefill),
            std::slice::from_ref(&same_prefill),
            1,
        ));
        assert!(rendered_history_is_prefix(&[repaired], &[canonical], 1));
        assert!(!rendered_history_is_prefix(
            &[user_call],
            &[user_call_reordered],
            1,
        ));
        assert!(!rendered_history_is_prefix(&[recorded], &[with_prose], 1));
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
    fn normalizes_qwen_thinking_and_maps_the_append_boundary() {
        let exact_history = format!(
            "system\n{QWEN_DISABLED_THINKING_PREFIX}tool call<|im_end|>\n{QWEN_DISABLED_THINKING_PREFIX}answer"
        );
        let normalized_history = normalize_assistant_context(&exact_history);
        let incoming = format!(
            "system\n{QWEN_ASSISTANT_HEADER}tool call<|im_end|>\n{QWEN_ASSISTANT_HEADER}answer<|im_end|>\nuser\n{QWEN_DISABLED_THINKING_PREFIX}"
        );
        let normalized_incoming = normalize_assistant_context(&incoming);

        assert!(normalized_incoming.starts_with(&normalized_history));
        let boundary =
            source_index_after_normalized_prefix(&incoming, normalized_history.len()).unwrap();
        assert_eq!(
            &incoming[boundary..],
            format!("<|im_end|>\nuser\n{QWEN_DISABLED_THINKING_PREFIX}")
        );
    }

    #[test]
    fn normalizes_gemma_thought_channel_and_maps_the_append_boundary() {
        let exact_history = format!(
            "system\n{GEMMA_THOUGHT_PREFIX}<|tool_call>call:fs.read{{}}<tool_call|><turn|>\n{GEMMA_THOUGHT_PREFIX}answer"
        );
        let normalized_history = normalize_assistant_context(&exact_history);
        let incoming = format!(
            "system\n<|tool_call>call:fs.read{{}}<tool_call|><turn|>\nanswer<turn|>\n<|turn>user\nnext\n{GEMMA_THOUGHT_PREFIX}"
        );
        let normalized_incoming = normalize_assistant_context(&incoming);

        assert!(normalized_incoming.starts_with(&normalized_history));
        let boundary =
            source_index_after_normalized_prefix(&incoming, normalized_history.len()).unwrap();
        assert_eq!(
            &incoming[boundary..],
            format!("<turn|>\n<|turn>user\nnext\n{GEMMA_THOUGHT_PREFIX}")
        );
    }

    #[test]
    fn strips_generated_gemma_thought_channel_from_public_content() {
        let mut content = format!("{GEMMA_THOUGHT_CHANNEL_PREFIX}G4V-7Q4M-9281");

        strip_generated_assistant_prefix(&mut content);

        assert_eq!(content, "G4V-7Q4M-9281");
    }

    #[test]
    fn normalizes_standalone_generated_gemma_thought_channel() {
        let source = format!("prompt{GEMMA_THOUGHT_CHANNEL_PREFIX}answer");
        let normalized = normalize_assistant_context(&source);

        assert_eq!(normalized, "promptanswer");
        assert_eq!(
            source_index_after_normalized_prefix(&source, normalized.len()),
            Some(source.len())
        );
    }

    #[test]
    fn prepared_chat_messages_use_llama_roles() {
        let rendered = RenderedModelRequest {
            run_id: agl_ids::RunId::generate(),
            turn_id: agl_ids::TurnId::generate(),
            request_index: 0,
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
            messages: vec![
                RenderedMessage {
                    role: RenderedMessageRole::System,
                    content: text("demo system"),
                    name: None,
                    tool_calls: Vec::new(),
                },
                RenderedMessage {
                    role: RenderedMessageRole::User,
                    content: text("hello"),
                    name: None,
                    tool_calls: Vec::new(),
                },
            ],
            tools: vec![RenderedTool {
                name: "unused".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            }],
        };

        let prepared =
            PreparedChatMessages::new(&rendered.messages, rendered.tool_call_format).unwrap();

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
    fn prepared_gemma_messages_preserve_tool_call_and_observation_fields() {
        let messages = vec![
            RenderedMessage {
                role: RenderedMessageRole::Assistant,
                content: text("<|tool_call>call:screen.capture{}<tool_call|>"),
                name: Some("screen.capture".to_string()),
                tool_calls: vec![RenderedToolCall {
                    name: "screen.capture".to_string(),
                    arguments: json!({}),
                }],
            },
            RenderedMessage {
                role: RenderedMessageRole::Tool,
                content: text(r#"{"status":"ok"}<__media__>"#),
                name: Some("screen.capture".to_string()),
                tool_calls: Vec::new(),
            },
        ];

        let prepared =
            PreparedChatMessages::new(&messages, ToolCallFormat::GemmaFunctionCall).unwrap();

        assert_eq!(prepared.messages.len(), 2);
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[0].content) }
                .to_str()
                .unwrap(),
            ""
        );
        assert_eq!(prepared.messages[0].n_tool_calls, 1);
        let prepared_tool_call = unsafe { &*prepared.messages[0].tool_calls };
        assert_eq!(
            unsafe { CStr::from_ptr(prepared_tool_call.name) }
                .to_str()
                .unwrap(),
            "screen.capture"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(prepared_tool_call.arguments) }
                .to_str()
                .unwrap(),
            "{}"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[1].name) }
                .to_str()
                .unwrap(),
            "screen.capture"
        );
        assert_eq!(
            unsafe { CStr::from_ptr(prepared.messages[1].content) }
                .to_str()
                .unwrap(),
            r#"{"status":"ok"}<__media__>"#
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
