use std::ffi::c_void;
use std::ptr;

use agl_config::InferenceRuntimeConfig;
use agl_oven::RenderedModelRequest;
use anyhow::{Context, Result, bail, ensure};

use super::super::{
    ffi,
    generation::{LlamaCppGenerationControl, LlamaCppGenerationOutput},
    model::LlamaCppModel,
};
use super::{LlamaCppContextSlot, decode::*, native::*, prompt::*};
use crate::InferenceFinishReason;

const AGL_LLAMA_MTP_OK: i32 = 0;

impl LlamaCppContextSlot {
    pub(super) fn generate_with_mtp(
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
    pub(super) fn generate_with_mtp_state(
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
}

pub(super) struct MtpState {
    speculative: MtpSpeculative,
    draft_context: ContextHandle,
    draft_tokens: usize,
}

impl MtpState {
    pub(super) fn new(
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

    pub(super) fn draft_context_ptr(&self) -> *mut c_void {
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
