use super::*;

pub(crate) struct Generated {
    pub(crate) output: ModelGenerationOutput,
    pub(crate) private_reasoning: Option<Content>,
    pub(crate) finish_reason: ModelFinishReason,
    pub(crate) usage: ModelUsage,
    pub(crate) timings: StreamTimings,
}

#[cfg(test)]
pub(crate) fn decode_generation(
    bytes: &[u8],
    attempt: &str,
    progress: Option<&InferenceProgressSink>,
    cancellation_requested: bool,
    reasoning_enabled: bool,
) -> Result<Generated, InferenceServiceError> {
    let mut decoder = GenerationStreamDecoder::new(attempt, progress.cloned(), reasoning_enabled);
    decoder.feed(bytes)?;
    decoder.finish(cancellation_requested)
}

pub(crate) struct GenerationStreamDecoder<'a> {
    attempt: &'a str,
    progress: Option<InferenceProgressSink>,
    pending: Vec<u8>,
    sequence: u64,
    terminal: Option<StreamFrame>,
    terminal_error: bool,
    raw_deltas: String,
    reasoning_enabled: bool,
}

impl<'a> GenerationStreamDecoder<'a> {
    pub(crate) fn new(
        attempt: &'a str,
        progress: Option<InferenceProgressSink>,
        reasoning_enabled: bool,
    ) -> Self {
        Self {
            attempt,
            progress,
            pending: Vec::new(),
            sequence: 1,
            terminal: None,
            terminal_error: false,
            raw_deltas: String::new(),
            reasoning_enabled,
        }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<(), InferenceServiceError> {
        if self.terminal.is_some() || self.terminal_error {
            return Err(InferenceServiceError::IdentityMismatch);
        }
        if self.pending.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
            return Err(InferenceServiceError::InvalidResult);
        }
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=end).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            self.accept_line(&line)?;
        }
        Ok(())
    }

    fn accept_line(&mut self, line: &[u8]) -> Result<(), InferenceServiceError> {
        let frame: StreamFrame = serde_json::from_slice(line).map_err(|error| {
            tracing::warn!(
                attempt_id=%self.attempt,
                %error,
                backend_diagnostic_bytes=line.len(),
                "private llama-server returned an invalid stream frame"
            );
            InferenceServiceError::InvalidResult
        })?;
        if frame.schema != "agentlibre.llama-stream/v1"
            || frame.attempt_id != self.attempt
            || frame.sequence != self.sequence
            || self.terminal.is_some()
            || self.terminal_error
        {
            return Err(InferenceServiceError::IdentityMismatch);
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(InferenceServiceError::InvalidRequest)?;
        match frame.kind.as_str() {
            "delta" => {
                if frame.finish_reason.is_some()
                    || frame.raw_output.is_some()
                    || frame.message.is_some()
                    || frame.usage.is_some()
                    || frame.prefill.is_some()
                    || frame.timings.is_some()
                    || frame.error.is_some()
                {
                    return Err(InferenceServiceError::InvalidResult);
                }
                let content = frame.content.ok_or(InferenceServiceError::InvalidResult)?;
                if content.is_empty() {
                    return Err(InferenceServiceError::InvalidResult);
                }
                self.raw_deltas.push_str(&content);
                if !self.reasoning_enabled
                    && let Some(progress) = &self.progress
                {
                    progress.emit(
                        Content::text(content).map_err(|_| InferenceServiceError::InvalidResult)?,
                    );
                }
            }
            "error" => {
                if frame.content.is_some()
                    || frame.finish_reason.is_some()
                    || frame.raw_output.is_some()
                    || frame.message.is_some()
                    || frame.usage.is_some()
                    || frame.prefill.is_some()
                    || frame.timings.is_some()
                    || frame.error.is_none()
                {
                    return Err(InferenceServiceError::InvalidResult);
                }
                let diagnostic = serde_json::to_vec(frame.error.as_ref().expect("checked above"))
                    .map(|value| bounded_backend_diagnostic(&value))
                    .unwrap_or_default();
                tracing::warn!(
                    attempt_id=%self.attempt,
                    backend_diagnostic_bytes=diagnostic.len(),
                    backend_diagnostic=%diagnostic,
                    "private llama-server generation failed"
                );
                self.terminal_error = true;
            }
            "final" => {
                if frame.content.is_some() || frame.error.is_some() {
                    return Err(InferenceServiceError::InvalidResult);
                }
                self.terminal = Some(frame);
            }
            _ => return Err(InferenceServiceError::InvalidRequest),
        }
        Ok(())
    }

    pub(crate) fn finish(
        self,
        cancellation_requested: bool,
    ) -> Result<Generated, InferenceServiceError> {
        if !self.pending.is_empty() {
            return Err(InferenceServiceError::OutcomeUnknown);
        }
        if self.terminal_error {
            return Err(if cancellation_requested {
                InferenceServiceError::Cancelled
            } else {
                InferenceServiceError::Unavailable
            });
        }
        if cancellation_requested {
            return Err(InferenceServiceError::OutcomeUnknown);
        }
        let frame = self.terminal.ok_or(InferenceServiceError::OutcomeUnknown)?;
        let usage = frame.usage.ok_or(InferenceServiceError::InvalidRequest)?;
        let timings = frame.timings.ok_or(InferenceServiceError::InvalidRequest)?;
        timings.validate(&usage)?;
        let raw = frame
            .raw_output
            .ok_or(InferenceServiceError::InvalidRequest)?;
        if self.raw_deltas != raw {
            return Err(InferenceServiceError::InvalidResult);
        }
        let usage = ModelUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        };
        let invalid_raw = Content::text(raw.clone()).ok();
        let invalid_output = |class, field: &str| {
            InferenceServiceError::InvalidModelOutput(Box::new(
                crate::inference::InvalidModelOutput {
                    raw_output: invalid_raw.clone(),
                    diagnostic: ModelOutputDiagnostic {
                        class,
                        field: Some(field.to_owned()),
                        finish_reason: match frame.finish_reason.as_deref() {
                            Some("length") => Some(ModelFinishReason::Length),
                            Some("tool_calls") => Some(ModelFinishReason::ToolCall),
                            Some("stop") => Some(ModelFinishReason::Stop),
                            _ => None,
                        },
                        output_bytes: raw.len() as u64,
                        output_digest: PackageDigest::from_bytes(
                            Sha256::digest(raw.as_bytes()).into(),
                        ),
                    },
                    usage,
                    realization: None,
                },
            ))
        };
        let (output, private_reasoning) = if self.reasoning_enabled {
            projected_reasoning_output(frame.message.as_ref()).map_err(|error| {
                invalid_output(
                    ModelOutputFailureClass::ReasoningProjection,
                    error.diagnostic_field(),
                )
            })?
        } else {
            (
                match structured_call(frame.message.as_ref()).map_err(|error| {
                    invalid_output(
                        ModelOutputFailureClass::StructuredToolCall,
                        error.diagnostic_field(),
                    )
                })? {
                    Some(call) => call,
                    None => parsed_output(&raw)
                        .map_err(|class| invalid_output(class, "raw_output.public_action"))?,
                },
                None,
            )
        };
        let finish_reason = match frame.finish_reason.as_deref() {
            Some("length") => ModelFinishReason::Length,
            Some("tool_calls") => ModelFinishReason::ToolCall,
            Some("stop")
                if matches!(
                    output,
                    ModelGenerationOutput::ToolCall(_)
                        | ModelGenerationOutput::ToolCalls(_)
                        | ModelGenerationOutput::AssistantToolCall(_)
                        | ModelGenerationOutput::AssistantToolCalls { .. }
                ) =>
            {
                ModelFinishReason::ToolCall
            }
            Some("stop") => ModelFinishReason::Stop,
            _ => return Err(InferenceServiceError::InvalidRequest),
        };
        Ok(Generated {
            output,
            private_reasoning,
            finish_reason,
            usage,
            timings,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelMessageError(String);

impl ModelMessageError {
    pub(crate) fn field(field: impl Into<String>) -> Self {
        Self(field.into())
    }

    fn diagnostic_field(&self) -> &str {
        &self.0
    }
}

pub(crate) fn projected_reasoning_output(
    message: Option<&Value>,
) -> Result<(ModelGenerationOutput, Option<Content>), ModelMessageError> {
    let message = message.ok_or_else(|| ModelMessageError::field("message.missing"))?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(ModelMessageError::field("message.role"));
    }
    if message
        .get("reasoning_content")
        .is_some_and(|value| !value.is_string())
    {
        return Err(ModelMessageError::field("message.reasoning_content.type"));
    }
    let content = match message.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(content)) if content.is_empty() => None,
        Some(Value::String(content)) => Some(content.as_str()),
        Some(_) => return Err(ModelMessageError::field("message.content.type")),
    };
    let private_reasoning = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(Content::text)
        .transpose()
        .map_err(|_| ModelMessageError::field("message.reasoning_content.value"))?;
    match (structured_call(Some(message))?, content) {
        (Some(call), None) => Ok((call, private_reasoning)),
        (None, Some(content)) => Content::text(content)
            .map(|content| (ModelGenerationOutput::Assistant(content), private_reasoning))
            .map_err(|_| ModelMessageError::field("message.content.value")),
        (None, None) => Err(ModelMessageError::field("message.public_action.none")),
        (Some(ModelGenerationOutput::ToolCall(call)), Some(content)) => Content::text(content)
            .map(|content| {
                (
                    ModelGenerationOutput::AssistantToolCall(agl_core::agent::AssistantToolCall {
                        content,
                        call,
                    }),
                    private_reasoning,
                )
            })
            .map_err(|_| ModelMessageError::field("message.content.value")),
        (Some(ModelGenerationOutput::ToolCalls(calls)), Some(content)) => Content::text(content)
            .map(|content| {
                (
                    ModelGenerationOutput::AssistantToolCalls { content, calls },
                    private_reasoning,
                )
            })
            .map_err(|_| ModelMessageError::field("message.content.value")),
        (Some(_), Some(_)) => unreachable!("structured_call returns only tool calls"),
    }
}

pub(crate) fn structured_call(
    message: Option<&Value>,
) -> Result<Option<ModelGenerationOutput>, ModelMessageError> {
    let Some(message) = message else {
        return Ok(None);
    };
    let Some(calls) = message.get("tool_calls") else {
        return Ok(None);
    };
    let calls = calls
        .as_array()
        .ok_or_else(|| ModelMessageError::field("message.tool_calls.type"))?;
    if calls.is_empty() {
        return Err(ModelMessageError::field(format!(
            "message.tool_calls.count={}",
            calls.len()
        )));
    }
    let parsed = calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let function = call
                .get("function")
                .or_else(|| call.get("agent"))
                .ok_or_else(|| {
                    ModelMessageError::field(format!("message.tool_calls[{index}].function"))
                })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ModelMessageError::field(format!("message.tool_calls[{index}].function.name"))
                })?;
            let arguments = function.get("arguments").ok_or_else(|| {
                ModelMessageError::field(format!("message.tool_calls[{index}].function.arguments"))
            })?;
            let arguments = match arguments {
                Value::String(value) => serde_json::from_str(value).map_err(|_| {
                    ModelMessageError::field(format!(
                        "message.tool_calls[{index}].function.arguments.json"
                    ))
                })?,
                value => value.clone(),
            };
            Ok(agl_core::agent::ToolCall {
                tool_id: agl_core::ToolId::new(name).map_err(|_| {
                    ModelMessageError::field(format!(
                        "message.tool_calls[{index}].function.name.value"
                    ))
                })?,
                input: arguments,
            })
        })
        .collect::<Result<Vec<_>, ModelMessageError>>()?;
    Ok(Some(if parsed.len() == 1 {
        ModelGenerationOutput::ToolCall(parsed.into_iter().next().unwrap())
    } else {
        ModelGenerationOutput::ToolCalls(parsed)
    }))
}

pub(crate) fn parsed_output(raw: &str) -> Result<ModelGenerationOutput, ModelOutputFailureClass> {
    match parse_model_output(raw) {
        ParsedModelOutput::Answer(answer) => Content::text(answer)
            .map(ModelGenerationOutput::Assistant)
            .map_err(|_| ModelOutputFailureClass::InvalidContent),
        ParsedModelOutput::ToolCall(call) => {
            Ok(ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                tool_id: agl_core::ToolId::new(call.name)
                    .map_err(|_| ModelOutputFailureClass::InvalidToolId)?,
                input: call.arguments,
            }))
        }
        ParsedModelOutput::ToolCalls(calls) => calls
            .into_iter()
            .map(|call| {
                Ok(agl_core::agent::ToolCall {
                    tool_id: agl_core::ToolId::new(call.name)
                        .map_err(|_| ModelOutputFailureClass::InvalidToolId)?,
                    input: call.arguments,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(ModelGenerationOutput::ToolCalls),
        ParsedModelOutput::MalformedToolCall(call) => match call.repair {
            Some(ToolJsonRepair::Succeeded { tool_call, .. }) => {
                Ok(ModelGenerationOutput::ToolCall(agl_core::agent::ToolCall {
                    tool_id: agl_core::ToolId::new(tool_call.name)
                        .map_err(|_| ModelOutputFailureClass::InvalidToolId)?,
                    input: tool_call.arguments,
                }))
            }
            _ => Err(match call.classification {
                crate::inference::output_codec::MalformedToolJsonKind::MissingTerminator => {
                    ModelOutputFailureClass::MissingTerminator
                }
                crate::inference::output_codec::MalformedToolJsonKind::Syntax => {
                    ModelOutputFailureClass::Syntax
                }
                crate::inference::output_codec::MalformedToolJsonKind::InvalidShape => {
                    ModelOutputFailureClass::InvalidShape
                }
            }),
        },
    }
}

pub(crate) fn verified_regular_file(path: &Path) -> Result<PathBuf, InferenceServiceError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| InferenceServiceError::InvalidRequest)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InferenceServiceError::InvalidRequest);
    }
    path.canonicalize()
        .map_err(|_| InferenceServiceError::InvalidRequest)
}

pub(crate) fn private_directory() -> Result<PathBuf, InferenceServiceError> {
    let path = std::env::temp_dir().join(format!(
        "agl-runtime-inference-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|_| InferenceServiceError::Unavailable)?;
    Ok(path)
}

pub(crate) fn address_space_limit(profile: &LlamaRuntimeProfile) -> u64 {
    profile
        .required_host_bytes
        .saturating_add(profile.required_device_bytes)
        .saturating_add(profile.required_shared_bytes)
        // llama.cpp temporarily duplicates prompt state while checkpointing a
        // slot. RLIMIT_AS bounds virtual mappings, not the admitted resident
        // memory, so leave room for that bounded transient copy.
        .saturating_mul(2)
        .saturating_add(2 * 1024 * 1024 * 1024)
}
