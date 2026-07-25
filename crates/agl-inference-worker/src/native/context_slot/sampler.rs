use std::ffi::{CString, c_void};

use agl_config::StructuredDecodingMode;
use anyhow::{Context, Result, bail, ensure};

use super::super::ffi;
use super::prompt::GenerationPlan;

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
