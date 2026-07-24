use std::ffi::{CString, c_void};
use std::ptr;

use agl_config::{KvCacheType, RuntimeSwitch, StructuredDecodingMode};
use anyhow::{Context, Result, bail, ensure};

use super::super::ffi;
use super::prompt::GenerationPlan;
pub(super) struct LlamaTokenBatch {
    tokens: Vec<ffi::llama_token>,
    positions: Vec<ffi::llama_pos>,
    n_seq_ids: Vec<i32>,
    _seq_ids: Vec<[ffi::llama_seq_id; 1]>,
    seq_id_ptrs: Vec<*mut ffi::llama_seq_id>,
    logits: Vec<i8>,
}

impl LlamaTokenBatch {
    pub(super) fn new(
        tokens: &[ffi::llama_token],
        start_pos: ffi::llama_pos,
        logits: bool,
    ) -> Result<Self> {
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

    pub(super) fn as_ffi(&mut self) -> ffi::llama_batch {
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

pub(super) struct ContextHandle(*mut c_void);

impl ContextHandle {
    pub(super) fn new(model: *mut c_void, params: ffi::llama_context_params) -> Result<Self> {
        let context = unsafe { ffi::llama_init_from_model(model, params) };
        ensure!(!context.is_null(), "llama.cpp returned null context");
        Ok(Self(context))
    }

    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for ContextHandle {
    fn drop(&mut self) {
        unsafe { ffi::llama_free(self.0) };
    }
}

pub(super) struct Sampler(*mut c_void);

impl Sampler {
    pub(super) fn greedy() -> Result<Self> {
        let params = unsafe { ffi::llama_sampler_chain_default_params() };
        let chain = unsafe { ffi::llama_sampler_chain_init(params) };
        ensure!(!chain.is_null(), "llama.cpp returned null sampler chain");
        let chain = Self(chain);
        let greedy = unsafe { ffi::llama_sampler_init_greedy() };
        if greedy.is_null() {
            bail!("llama.cpp returned null greedy sampler");
        }
        unsafe { ffi::llama_sampler_chain_add(chain.as_ptr(), greedy) };
        Ok(chain)
    }

    pub(super) fn for_generation(
        vocab: *const c_void,
        plan: Option<&GenerationPlan>,
        mode: StructuredDecodingMode,
    ) -> Result<Self> {
        if mode == StructuredDecodingMode::Off {
            return Self::greedy();
        }
        let Some(plan) = plan.filter(|plan| !plan.grammar.is_empty()) else {
            return Self::greedy();
        };
        let params = unsafe { ffi::llama_sampler_chain_default_params() };
        let chain = unsafe { ffi::llama_sampler_chain_init(params) };
        ensure!(!chain.is_null(), "llama.cpp returned null sampler chain");
        let chain = Self(chain);

        let grammar_text =
            CString::new(plan.grammar.as_str()).context("llama.cpp grammar contains NUL")?;
        let grammar_root = c"root";
        let grammar = if plan.grammar_lazy {
            let (patterns, tokens) = grammar_trigger_inputs(plan)?;
            let pattern_ptrs = patterns
                .iter()
                .map(|pattern| pattern.as_ptr())
                .collect::<Vec<_>>();
            unsafe {
                ffi::llama_sampler_init_grammar_lazy_patterns(
                    vocab,
                    grammar_text.as_ptr(),
                    grammar_root.as_ptr(),
                    pattern_ptrs.as_ptr(),
                    pattern_ptrs.len(),
                    tokens.as_ptr(),
                    tokens.len(),
                )
            }
        } else {
            unsafe {
                ffi::llama_sampler_init_grammar(vocab, grammar_text.as_ptr(), grammar_root.as_ptr())
            }
        };
        if grammar.is_null() {
            bail!("llama.cpp rejected the generation grammar");
        }
        if plan.grammar_needs_prefill {
            for token in &plan.grammar_prefill_tokens {
                unsafe { ffi::llama_sampler_accept(grammar, *token) };
            }
        }
        unsafe { ffi::llama_sampler_chain_add(chain.as_ptr(), grammar) };

        let greedy = unsafe { ffi::llama_sampler_init_greedy() };
        if greedy.is_null() {
            bail!("llama.cpp returned null greedy sampler");
        }
        unsafe { ffi::llama_sampler_chain_add(chain.as_ptr(), greedy) };
        Ok(chain)
    }

    pub(super) fn try_clone(&self) -> Result<Self> {
        let sampler = unsafe { ffi::llama_sampler_clone(self.0.cast_const()) };
        ensure!(
            !sampler.is_null(),
            "llama.cpp failed to clone sampler state"
        );
        Ok(Self(sampler))
    }

    pub(super) fn accept(&self, token: ffi::llama_token) {
        unsafe { ffi::llama_sampler_accept(self.0, token) };
    }

    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

pub(super) fn grammar_trigger_inputs(
    plan: &GenerationPlan,
) -> Result<(Vec<CString>, Vec<ffi::llama_token>)> {
    let mut patterns = Vec::new();
    let mut tokens = Vec::new();
    for trigger in &plan.grammar_triggers {
        let pattern = match trigger.kind {
            0 => {
                tokens.push(trigger.token);
                continue;
            }
            1 => regex_escape(&trigger.value),
            2 => trigger.value.clone(),
            3 if trigger.value.is_empty() => "^$".to_string(),
            3 => format!(
                "{}{}{}",
                if trigger.value.starts_with('^') {
                    ""
                } else {
                    "^"
                },
                trigger.value,
                if trigger.value.ends_with('$') {
                    ""
                } else {
                    "$"
                },
            ),
            kind => bail!("unsupported llama.cpp grammar trigger type {kind}"),
        };
        patterns.push(CString::new(pattern).context("grammar trigger contains NUL")?);
    }
    ensure!(
        !plan.grammar_lazy || !patterns.is_empty() || !tokens.is_empty(),
        "lazy grammar has no trigger patterns or tokens"
    );
    Ok((patterns, tokens))
}

fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe { ffi::llama_sampler_free(self.0) };
    }
}

pub(super) fn context_next_pos(ctx: *mut c_void) -> ffi::llama_pos {
    (unsafe { ffi::llama_memory_seq_pos_max(ffi::llama_get_memory(ctx), 0) }) + 1
}

pub(super) fn context_remaining(ctx: *mut c_void) -> ffi::llama_pos {
    (unsafe { ffi::llama_n_ctx(ctx) } as i32).saturating_sub(context_next_pos(ctx))
}

pub(super) fn has_context_space(ctx: *mut c_void, requested_tokens: i32) -> bool {
    let used = unsafe { ffi::llama_memory_seq_pos_max(ffi::llama_get_memory(ctx), 0) } + 1;
    used.saturating_add(requested_tokens) < unsafe { ffi::llama_n_ctx(ctx) } as i32
}

pub(super) fn history_len_as_pos(history: &[ffi::llama_token]) -> Result<ffi::llama_pos> {
    i32::try_from(history.len()).context("llama.cpp token history exceeds i32")
}

pub(super) fn map_flash_attention(value: Option<RuntimeSwitch>) -> i32 {
    match value {
        Some(RuntimeSwitch::On) => ffi::LLAMA_FLASH_ATTN_TYPE_ENABLED,
        Some(RuntimeSwitch::Off) => ffi::LLAMA_FLASH_ATTN_TYPE_DISABLED,
        Some(RuntimeSwitch::Auto) | None => ffi::LLAMA_FLASH_ATTN_TYPE_AUTO,
    }
}

pub(super) fn map_cache_type(value: KvCacheType) -> i32 {
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
