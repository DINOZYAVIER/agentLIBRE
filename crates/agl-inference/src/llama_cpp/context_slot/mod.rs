mod decode;
mod mtp;
mod native;
mod prompt;

#[cfg(test)]
mod tests;

use std::marker::PhantomData;
use std::rc::Rc;

use agl_config::{InferenceRuntimeConfig, ResolvedInferenceConfig};
use agl_oven::{RenderedMessage, RenderedModelRequest};
use anyhow::{Context, Result, ensure};

use super::ffi;
use super::generation::{
    LlamaCppGenerationCancelled, LlamaCppGenerationControl, LlamaCppGenerationOutput,
};
use super::model::LlamaCppModel;
use mtp::MtpState;
use native::{ContextHandle, Sampler, map_cache_type, map_flash_attention};
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
        config: &ResolvedInferenceConfig,
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

    pub(crate) fn matches_config(&self, config: &ResolvedInferenceConfig) -> bool {
        self.runtime == config.runtime
    }

    pub(crate) fn reset_cache(
        &mut self,
        model: &LlamaCppModel,
        config: &ResolvedInferenceConfig,
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
}
