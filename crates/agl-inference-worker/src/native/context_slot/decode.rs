use std::ffi::{CString, c_void};
use std::ptr;

use agl_oven::{RenderedMessage, RenderedModelRequest};
use anyhow::{Context, Result, ensure};

use super::super::{
    ffi,
    generation::{LlamaCppGenerationControl, LlamaCppGenerationOutput},
    model::LlamaCppModel,
};
use super::output::IncrementalResponseClassifier;
use super::{LlamaCppContextSlot, LlamaCppGenerationRequest, native::*, prompt::*};
use agl_inference::InferenceFinishReason;

impl LlamaCppContextSlot {
    pub(super) fn generate_inner(
        &mut self,
        model: &LlamaCppModel,
        request: LlamaCppGenerationRequest<'_>,
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
    ) -> Result<LlamaCppGenerationOutput> {
        let prepared = self.prepare_prompt_append(model, request.rendered, log)?;

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
            return self.generate_with_mtp(model, request, prepared, control, log);
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
            request.rendered,
            prompt_messages,
            prompt_history,
            input_tokens,
            request.max_output_tokens,
            request.classifier(),
            control,
        )
    }

    pub(super) fn generate_vision_inner(
        &mut self,
        model: &LlamaCppModel,
        request: LlamaCppGenerationRequest<'_>,
        images: &[&[u8]],
        control: &LlamaCppGenerationControl<'_>,
        log: &mut String,
    ) -> Result<LlamaCppGenerationOutput> {
        ensure!(
            self.cache.rendered_message_history_len == 0 && self.cache.token_history.is_empty(),
            "llama.cpp vision requires a fresh context"
        );
        let prepared = self.prepare_prompt_append(model, request.rendered, log)?;
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
            request.rendered,
            prompt_messages,
            prompt_history,
            u64::try_from(input_tokens).unwrap_or(u64::MAX),
            request.max_output_tokens,
            request.classifier(),
            control,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn generate_after_prefill(
        &mut self,
        model: &LlamaCppModel,
        rendered: &RenderedModelRequest,
        prompt_messages: Vec<RenderedMessage>,
        prompt_history: PreparedPromptHistory,
        input_tokens: u64,
        max_output_tokens: u32,
        mut output: IncrementalResponseClassifier<'_>,
        control: &LlamaCppGenerationControl<'_>,
    ) -> Result<LlamaCppGenerationOutput> {
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

            let piece = token_to_piece_bytes(model.vocab(), token)?;
            let mut next_token = [token];
            decode_tokens(self.context.as_ptr(), &mut next_token)
                .context("failed to decode generated token")?;
            self.cache.token_history.push(token);
            output_tokens = output_tokens.saturating_add(1);
            if output.push(&piece) {
                finish_reason = if output.content_byte_limit_reached() {
                    InferenceFinishReason::ContentByteLimit
                } else {
                    InferenceFinishReason::Stop
                };
                break;
            }
            if output.stopped_on_continuation() {
                finish_reason = InferenceFinishReason::Stop;
                break;
            }
        }

        let response = output.finish();
        if response.content_byte_limit_reached {
            finish_reason = InferenceFinishReason::ContentByteLimit;
        }

        self.record_generated_assistant(
            rendered,
            prompt_messages,
            prompt_history,
            &response.decoded_content,
            &response.content,
        )?;

        Ok(LlamaCppGenerationOutput {
            content: response.content,
            finish_reason,
            input_tokens,
            output_tokens,
        })
    }
}

pub(super) fn tokenize(
    vocab: *const c_void,
    text: &str,
    add_special: bool,
) -> Result<Vec<ffi::llama_token>> {
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

pub(super) fn decode_tokens(ctx: *mut c_void, tokens: &mut [ffi::llama_token]) -> Result<()> {
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

pub(super) fn decode_explicit_tokens(
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

pub(super) fn decode_prompt_tokens(
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

pub(super) fn prefill_chunk_count(token_count: usize, batch_size: usize) -> Result<usize> {
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

pub(super) fn token_to_piece_bytes(
    vocab: *const c_void,
    token: ffi::llama_token,
) -> Result<Vec<u8>> {
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
        return piece_buf_to_bytes(&buf, len);
    }
    piece_buf_to_bytes(&buf, len)
}

pub(super) fn piece_buf_to_bytes(buf: &[i8], len: i32) -> Result<Vec<u8>> {
    let len = usize::try_from(len).context("invalid llama.cpp piece length")?;
    Ok(buf[..len].iter().map(|value| *value as u8).collect())
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
