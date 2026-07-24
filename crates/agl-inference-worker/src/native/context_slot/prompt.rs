use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

use agl_actions::{ParsedModelOutput, ToolCall, ToolJsonRepair, parse_model_output};
use agl_config::{StructuredDecodingMode, ToolCallFormat};
use agl_content::Content;
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest, RenderedTool};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::super::{ffi, model::LlamaCppModel};
use super::LlamaCppContextSlot;
use super::decode::tokenize;

pub(super) const DISABLED_THINKING_PREFILL: &str = "<think>\n\n</think>\n\n";
pub(super) const QWEN_ASSISTANT_HEADER: &str = "<|im_start|>assistant\n";
pub(super) const QWEN_DISABLED_THINKING_PREFIX: &str =
    "<|im_start|>assistant\n<think>\n\n</think>\n\n";
pub(super) const GEMMA_MODEL_HEADER: &str = "<|turn>model\n";
pub(super) const GEMMA_THOUGHT_CHANNEL_PREFIX: &str = "<|channel>thought\n<channel|>";
pub(super) const GEMMA_THOUGHT_PREFIX: &str = "<|turn>model\n<|channel>thought\n<channel|>";

impl LlamaCppContextSlot {
    pub(super) fn render_prompt_append(
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
        if rendered.tool_call_format == ToolCallFormat::GemmaFunctionCall {
            restore_cached_gemma_tool_calls(
                &mut messages,
                &rendered.messages,
                self.cache.rendered_message_history_len,
            );
        }
        messages.extend(
            rendered.messages[self.cache.rendered_message_history_len..]
                .iter()
                .cloned(),
        );
        let generation_plan = if rendered.tools.is_empty() {
            None
        } else {
            let plan = build_common_generation_plan(
                model.main_ptr().cast_const(),
                &messages,
                &rendered.tools,
            )
            .map_err(anyhow::Error::new)
            .context("failed to build llama.cpp structured generation plan")?;
            if self.runtime.structured_decoding == StructuredDecodingMode::Required
                && plan.grammar.is_empty()
            {
                return Err(GenerationPlanError::MissingGrammar.into());
            }
            Some(plan)
        };
        let mut formatted = match &generation_plan {
            Some(plan) => plan.prompt.clone(),
            None => apply_chat_template_messages(
                model.main_ptr().cast_const(),
                &messages,
                rendered.tool_call_format,
                true,
            )
            .context("failed to render llama.cpp chat template")?,
        };
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
            generation_plan,
            history: PreparedPromptHistory {
                assistant_context_prefix,
                formatted_prompt,
            },
            messages,
        })
    }

    pub(super) fn prepare_prompt_append(
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
            generation_plan,
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
        log.push_str("structured_decoding_mode = ");
        log.push_str(match self.runtime.structured_decoding {
            StructuredDecodingMode::Off => "off",
            StructuredDecodingMode::Auto => "auto",
            StructuredDecodingMode::Required => "required",
        });
        log.push('\n');
        log.push_str("repair_malformed_tool_calls = ");
        log.push_str(if self.runtime.repair_malformed_tool_calls {
            "true"
        } else {
            "false"
        });
        log.push('\n');
        log.push_str("tool_set_digest = ");
        log.push_str(&tool_set_digest(&rendered.tools)?);
        log.push('\n');
        if let Some(plan) = &generation_plan {
            log.push_str("grammar_digest = ");
            log.push_str(&hex_digest(plan.grammar.as_bytes()));
            log.push('\n');
            log.push_str("grammar_lazy = ");
            log.push_str(if plan.grammar_lazy { "true" } else { "false" });
            log.push('\n');
            log.push_str("common_chat_format = ");
            log.push_str(&plan.format.to_string());
            log.push('\n');
            log.push_str("common_chat_parser_digest = ");
            log.push_str(&hex_digest(plan.parser.as_bytes()));
            log.push('\n');
        } else {
            log.push_str("grammar_digest = none\n");
            log.push_str("grammar_lazy = false\n");
        }
        log.push_str("llama_cpp_prompt_append:\n");
        log.push_str(&prompt);
        if !prompt.ends_with('\n') {
            log.push('\n');
        }

        Ok(PreparedPrompt {
            tokens: prompt_tokens,
            generation_plan,
            messages,
            history,
        })
    }

    pub(super) fn record_generated_assistant(
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

pub(super) fn restore_cached_gemma_tool_calls(
    cached: &mut [RenderedMessage],
    incoming: &[RenderedMessage],
    history_len: usize,
) {
    for (cached, incoming) in
        cached
            .iter_mut()
            .zip(incoming)
            .take(history_len)
            .filter(|(_, incoming)| {
                incoming.role == RenderedMessageRole::Assistant && !incoming.tool_calls.is_empty()
            })
    {
        cached.name.clone_from(&incoming.name);
        cached.tool_calls.clone_from(&incoming.tool_calls);
    }
}

pub(super) fn rendered_history_message_matches(
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

pub(super) fn isolated_tool_call(content: &str) -> Option<ToolCall> {
    isolated_tool_call_with_repair(content, true)
}

pub(super) fn isolated_tool_call_with_repair(
    content: &str,
    repair_malformed: bool,
) -> Option<ToolCall> {
    let content = content.trim();
    let isolated = isolated_block(content, "<tool_call>", "</tool_call>")
        || isolated_block(content, "<|tool_call>", "<tool_call|>");
    if !isolated {
        return None;
    }
    match parse_model_output(content) {
        ParsedModelOutput::ToolCall(tool_call) => Some(tool_call),
        ParsedModelOutput::MalformedToolCall(malformed) if repair_malformed => {
            match malformed.repair {
                Some(ToolJsonRepair::Succeeded { tool_call, .. }) => Some(tool_call),
                Some(ToolJsonRepair::Failed { .. }) | None => None,
            }
        }
        ParsedModelOutput::MalformedToolCall(_) => None,
        ParsedModelOutput::Answer(_) => None,
    }
}

pub(super) fn isolated_block(content: &str, open: &str, close: &str) -> bool {
    content
        .strip_prefix(open)
        .and_then(|content| content.strip_suffix(close))
        .is_some_and(|content| !content.contains(open) && !content.contains(close))
}

pub(super) fn common_prefix_len(recorded: &str, incoming: &str) -> usize {
    recorded
        .bytes()
        .zip(incoming.bytes())
        .take_while(|(recorded, incoming)| recorded == incoming)
        .count()
}

pub(super) fn normalize_assistant_context(value: &str) -> String {
    value
        .replace(QWEN_DISABLED_THINKING_PREFIX, QWEN_ASSISTANT_HEADER)
        .replace(GEMMA_THOUGHT_PREFIX, "")
        .replace(GEMMA_THOUGHT_CHANNEL_PREFIX, "")
        .replace(GEMMA_MODEL_HEADER, "")
}

pub(super) fn strip_generated_assistant_prefix(content: &mut String) {
    for prefix in [
        GEMMA_THOUGHT_PREFIX,
        GEMMA_THOUGHT_CHANNEL_PREFIX,
        GEMMA_MODEL_HEADER,
    ] {
        if content.starts_with(prefix) {
            content.drain(..prefix.len());
        }
    }
    if let Some(stripped) = content.strip_prefix("Assistant:") {
        *content = stripped.trim_start().to_string();
    }
}

pub(super) fn generated_assistant_prefix_is_pending(content: &str) -> bool {
    [
        GEMMA_THOUGHT_PREFIX,
        GEMMA_THOUGHT_CHANNEL_PREFIX,
        GEMMA_MODEL_HEADER,
        "Assistant:",
    ]
    .iter()
    .any(|prefix| prefix.starts_with(content) && content.len() < prefix.len())
}

pub(super) fn source_index_after_normalized_prefix(
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

pub(super) fn assistant_context_rewrite(value: &str) -> Option<(usize, usize)> {
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

pub(super) fn mismatch_excerpt(value: &str, offset: usize) -> String {
    let bytes = &value.as_bytes()[offset.min(value.len())..];
    String::from_utf8_lossy(&bytes[..bytes.len().min(160)]).into_owned()
}

pub(super) struct PreparedPrompt {
    pub(super) tokens: Vec<ffi::llama_token>,
    pub(super) generation_plan: Option<GenerationPlan>,
    pub(super) messages: Vec<RenderedMessage>,
    pub(super) history: PreparedPromptHistory,
}

pub(super) struct PreparedPromptHistory {
    pub(super) assistant_context_prefix: String,
    pub(super) formatted_prompt: String,
}

pub(super) struct PromptTemplateAppend {
    prompt: String,
    tokens: Vec<ffi::llama_token>,
    generation_plan: Option<GenerationPlan>,
    history: PreparedPromptHistory,
    messages: Vec<RenderedMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GenerationPlan {
    pub(super) prompt: String,
    pub(super) grammar: String,
    pub(super) grammar_lazy: bool,
    pub(super) grammar_needs_prefill: bool,
    pub(super) grammar_triggers: Vec<GrammarTrigger>,
    pub(super) grammar_prefill_tokens: Vec<ffi::llama_token>,
    pub(super) additional_stops: Vec<String>,
    pub(super) preserved_tokens: Vec<String>,
    pub(super) generation_prompt: String,
    pub(super) format: i32,
    pub(super) parser: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(super) struct GrammarTrigger {
    #[serde(rename = "type")]
    pub(super) kind: i32,
    pub(super) value: String,
    pub(super) token: ffi::llama_token,
}

#[derive(Debug)]
pub(super) enum GenerationPlanError {
    InvalidToolSchema { tool: String, reason: String },
    Boundary(String),
    InvalidUtf8(&'static str),
    InvalidMetadata { field: &'static str, reason: String },
    MissingGrammar,
}

impl std::fmt::Display for GenerationPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidToolSchema { tool, reason } => {
                write!(formatter, "invalid Tool schema for `{tool}`: {reason}")
            }
            Self::Boundary(reason) => {
                write!(formatter, "llama.cpp generation plan failed: {reason}")
            }
            Self::InvalidUtf8(field) => {
                write!(
                    formatter,
                    "llama.cpp generation plan `{field}` is not UTF-8"
                )
            }
            Self::InvalidMetadata { field, reason } => {
                write!(formatter, "invalid generation plan `{field}`: {reason}")
            }
            Self::MissingGrammar => {
                formatter.write_str("structured decoding is required but the Tool grammar is empty")
            }
        }
    }
}

impl std::error::Error for GenerationPlanError {}

pub(super) struct PreparedTools {
    _names: Vec<CString>,
    _descriptions: Vec<CString>,
    _parameters: Vec<CString>,
    pub(super) tools: Vec<ffi::agl_llama_chat_tool>,
}

impl PreparedTools {
    pub(super) fn new(tools: &[RenderedTool]) -> std::result::Result<Self, GenerationPlanError> {
        let mut names = Vec::with_capacity(tools.len());
        let mut descriptions = Vec::with_capacity(tools.len());
        let mut parameters = Vec::with_capacity(tools.len());
        let mut ffi_tools = Vec::with_capacity(tools.len());
        for tool in tools {
            if !tool.input_schema.is_object() {
                return Err(GenerationPlanError::InvalidToolSchema {
                    tool: tool.name.clone(),
                    reason: "Tool input schema root must be an object".to_string(),
                });
            }
            names.push(CString::new(tool.name.as_str()).map_err(|error| {
                GenerationPlanError::InvalidToolSchema {
                    tool: tool.name.clone(),
                    reason: error.to_string(),
                }
            })?);
            descriptions.push(CString::new(tool.description.as_str()).map_err(|error| {
                GenerationPlanError::InvalidToolSchema {
                    tool: tool.name.clone(),
                    reason: error.to_string(),
                }
            })?);
            let schema = serde_json::to_string(&tool.input_schema).map_err(|error| {
                GenerationPlanError::InvalidToolSchema {
                    tool: tool.name.clone(),
                    reason: error.to_string(),
                }
            })?;
            parameters.push(CString::new(schema).map_err(|error| {
                GenerationPlanError::InvalidToolSchema {
                    tool: tool.name.clone(),
                    reason: error.to_string(),
                }
            })?);
            ffi_tools.push(ffi::agl_llama_chat_tool {
                name: names.last().expect("name was pushed").as_ptr(),
                description: descriptions
                    .last()
                    .expect("description was pushed")
                    .as_ptr(),
                parameters: parameters.last().expect("parameters were pushed").as_ptr(),
            });
        }
        Ok(Self {
            _names: names,
            _descriptions: descriptions,
            _parameters: parameters,
            tools: ffi_tools,
        })
    }
}

struct RawGenerationPlan(ffi::agl_llama_generation_plan);

impl RawGenerationPlan {
    fn empty() -> Self {
        Self(ffi::agl_llama_generation_plan {
            prompt: ptr::null_mut(),
            prompt_len: 0,
            grammar: ptr::null_mut(),
            grammar_len: 0,
            grammar_lazy: 0,
            grammar_needs_prefill: 0,
            grammar_triggers_json: ptr::null_mut(),
            grammar_triggers_json_len: 0,
            grammar_prefill_tokens_json: ptr::null_mut(),
            grammar_prefill_tokens_json_len: 0,
            additional_stops_json: ptr::null_mut(),
            additional_stops_json_len: 0,
            preserved_tokens_json: ptr::null_mut(),
            preserved_tokens_json_len: 0,
            generation_prompt: ptr::null_mut(),
            generation_prompt_len: 0,
            format: 0,
            parser: ptr::null_mut(),
            parser_len: 0,
        })
    }
}

impl Drop for RawGenerationPlan {
    fn drop(&mut self) {
        unsafe { ffi::agl_llama_generation_plan_free(ptr::from_mut(&mut self.0)) };
    }
}

fn build_common_generation_plan(
    model: *const c_void,
    messages: &[RenderedMessage],
    tools: &[RenderedTool],
) -> std::result::Result<GenerationPlan, GenerationPlanError> {
    if messages.len() > isize::MAX as usize || tools.len() > isize::MAX as usize {
        return Err(GenerationPlanError::Boundary(
            "input count exceeds the FFI addressable range".to_string(),
        ));
    }
    let prepared_messages = PreparedChatMessages::new_structured(messages)
        .map_err(|error| GenerationPlanError::Boundary(error.to_string()))?;
    let prepared_tools = PreparedTools::new(tools)?;
    let mut raw = RawGenerationPlan::empty();
    let mut error = vec![0_i8; 4096];
    let status = unsafe {
        ffi::agl_llama_common_chat_build_generation_plan(
            model,
            prepared_messages.messages.as_ptr(),
            prepared_messages.messages.len(),
            prepared_tools.tools.as_ptr(),
            prepared_tools.tools.len(),
            true,
            ptr::from_mut(&mut raw.0),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if status != 0 {
        return Err(GenerationPlanError::Boundary(c_error_message(&error)));
    }
    let prompt = raw_string(raw.0.prompt, raw.0.prompt_len, "prompt")?;
    if prompt.is_empty() {
        return Err(GenerationPlanError::Boundary(
            "common chat template returned an empty prompt".to_string(),
        ));
    }
    let triggers_json = raw_string(
        raw.0.grammar_triggers_json,
        raw.0.grammar_triggers_json_len,
        "grammar_triggers",
    )?;
    let prefill_json = raw_string(
        raw.0.grammar_prefill_tokens_json,
        raw.0.grammar_prefill_tokens_json_len,
        "grammar_prefill_tokens",
    )?;
    let stops_json = raw_string(
        raw.0.additional_stops_json,
        raw.0.additional_stops_json_len,
        "additional_stops",
    )?;
    let preserved_json = raw_string(
        raw.0.preserved_tokens_json,
        raw.0.preserved_tokens_json_len,
        "preserved_tokens",
    )?;
    Ok(GenerationPlan {
        prompt,
        grammar: raw_string(raw.0.grammar, raw.0.grammar_len, "grammar")?,
        grammar_lazy: raw.0.grammar_lazy != 0,
        grammar_needs_prefill: raw.0.grammar_needs_prefill != 0,
        grammar_triggers: parse_plan_json(&triggers_json, "grammar_triggers")?,
        grammar_prefill_tokens: parse_plan_json(&prefill_json, "grammar_prefill_tokens")?,
        additional_stops: parse_plan_json(&stops_json, "additional_stops")?,
        preserved_tokens: parse_plan_json(&preserved_json, "preserved_tokens")?,
        generation_prompt: raw_string(
            raw.0.generation_prompt,
            raw.0.generation_prompt_len,
            "generation_prompt",
        )?,
        format: raw.0.format,
        parser: raw_string(raw.0.parser, raw.0.parser_len, "parser")?,
    })
}

pub(super) fn raw_string(
    value: *const c_char,
    length: usize,
    field: &'static str,
) -> std::result::Result<String, GenerationPlanError> {
    if value.is_null() {
        return Err(GenerationPlanError::Boundary(format!(
            "generation plan `{field}` pointer is null"
        )));
    }
    if length > isize::MAX as usize {
        return Err(GenerationPlanError::Boundary(format!(
            "generation plan `{field}` length exceeds isize"
        )));
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    if bytes.len() != length {
        return Err(GenerationPlanError::Boundary(format!(
            "generation plan `{field}` length mismatch"
        )));
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| GenerationPlanError::InvalidUtf8(field))
}

fn parse_plan_json<T: serde::de::DeserializeOwned>(
    value: &str,
    field: &'static str,
) -> std::result::Result<T, GenerationPlanError> {
    serde_json::from_str(value).map_err(|error| GenerationPlanError::InvalidMetadata {
        field,
        reason: error.to_string(),
    })
}

fn tool_set_digest(tools: &[RenderedTool]) -> Result<String> {
    Ok(hex_digest(&serde_json::to_vec(tools)?))
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn apply_chat_template_messages(
    model: *const c_void,
    messages: &[RenderedMessage],
    tool_call_format: ToolCallFormat,
    add_assistant: bool,
) -> Result<String> {
    let prepared = PreparedChatMessages::new(messages, tool_call_format)?;
    apply_common_chat_template(model, &prepared, add_assistant)
}

pub(super) fn apply_common_chat_template(
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

pub(super) fn c_error_message(buf: &[c_char]) -> String {
    if buf.first().copied().unwrap_or_default() == 0 {
        return "unknown error".to_string();
    }

    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub(super) struct PreparedChatMessages {
    _roles: Vec<CString>,
    _contents: Vec<CString>,
    _names: Vec<CString>,
    _tool_call_names: Vec<CString>,
    _tool_call_arguments: Vec<CString>,
    _tool_calls: Vec<Vec<ffi::agl_llama_chat_tool_call>>,
    pub(super) messages: Vec<ffi::agl_llama_chat_message>,
}

impl PreparedChatMessages {
    pub(super) fn new(
        messages: &[RenderedMessage],
        tool_call_format: ToolCallFormat,
    ) -> Result<Self> {
        Self::with_structured_fields(
            messages,
            tool_call_format == ToolCallFormat::GemmaFunctionCall,
        )
    }

    fn new_structured(messages: &[RenderedMessage]) -> Result<Self> {
        Self::with_structured_fields(messages, true)
    }

    fn with_structured_fields(
        messages: &[RenderedMessage],
        render_structured_tool_fields: bool,
    ) -> Result<Self> {
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

pub(super) fn rendered_message_content(message: &RenderedMessage) -> Result<String> {
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

pub(super) fn disable_qwen_thinking(prompt: &mut String) -> Option<&'static str> {
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
