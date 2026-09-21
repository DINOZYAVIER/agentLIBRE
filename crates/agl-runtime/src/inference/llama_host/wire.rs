use super::*;

pub(crate) fn native_request(
    request: &crate::inference::request_codec::RenderedModelRequest,
    attempt: &str,
    source: &InferenceGenerateRequest,
) -> Result<Vec<u8>, InferenceServiceError> {
    let mut messages = request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                RenderedMessageRole::System => "system",
                RenderedMessageRole::User => "user",
                RenderedMessageRole::Assistant => "assistant",
                RenderedMessageRole::Tool => "tool",
            };
            let content = message
                .content
                .as_ref()
                .map(|content| content.as_text().to_owned());
            let mut value = json!({"role": role, "content": content});
            if let Some(name) = &message.name {
                value["name"] = json!(name);
            }
            if let Some(reasoning) = &message.private_reasoning {
                value["reasoning_content"] = json!(reasoning.as_text());
            }
            if let Some(call) = &message.tool_call {
                value["tool_calls"] = json!([{"id":"call_0","type":"function","function":{
                    "name":call.name,"arguments":call.arguments.to_string()
                }}]);
            } else if !message.tool_calls.is_empty() {
                value["tool_calls"] = json!(
                    message
                        .tool_calls
                        .iter()
                        .enumerate()
                        .map(|(index, call)| json!({
                            "id": format!("call_{index}"),
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments.to_string()
                            }
                        }))
                        .collect::<Vec<_>>()
                );
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, InferenceServiceError>>()?;
    let after_compaction = source.context.last().is_some_and(|entry| {
        matches!(
            entry.source_request,
            Some(agl_core::agent::AgentOperationRequest::Compaction(_))
        )
    });
    append_user_turn_after_assistant(&mut messages, after_compaction);
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let parameters = llama_tool_schema(tool.input_schema.clone());
            json!({
                "type":"function","function":{"name":tool.name,"description":tool.description,
                "parameters":parameters}
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "agl_attempt_id": attempt,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "stream": true,
        "seed": source.runtime.generation.seed,
        "temperature": source.runtime.generation.temperature,
        "top_k": source.runtime.generation.top_k,
        "top_p": source.runtime.generation.top_p,
        "min_p": source.runtime.generation.min_p,
        "typical_p": source.runtime.generation.typical_p,
        "repeat_last_n": source.runtime.generation.repeat_last_n,
        "repeat_penalty": source.runtime.generation.repeat_penalty,
        "presence_penalty": source.runtime.generation.presence_penalty,
        "frequency_penalty": source.runtime.generation.frequency_penalty,
        "stop": source.runtime.generation.stop,
        "max_tokens": request.max_output_tokens.min(source.runtime.generation.max_output_tokens),
        "id_slot": -1,
        "cache_prompt": true
    });
    apply_reasoning_request(&mut body, source.runtime.reasoning);
    if let Some(response_format) = &request.response_format {
        body["response_format"] = response_format.clone();
    }
    serde_json::to_vec(&body).map_err(|_| InferenceServiceError::InvalidRequest)
}

pub(crate) fn append_user_turn_after_assistant(messages: &mut Vec<Value>, after_compaction: bool) {
    if messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
    {
        let content = if after_compaction {
            "Continue the task preserved in the compaction summary, starting from the concrete next step in semantic.next_position. Use exact.inspected_files and semantic.completed as the work ledger; do not repeat completed investigation unless a file digest changed."
        } else {
            ""
        };
        messages.push(json!({"role": "user", "content": content}));
    }
}

pub(crate) fn llama_tool_schema(mut value: Value) -> Value {
    if let Value::Object(object) = &mut value {
        project_root_object_union(object);
    }
    relax_llama_repetition_bounds(&mut value);
    value
}

pub(crate) fn relax_llama_repetition_bounds(value: &mut Value) {
    const MAX_GBNF_REPETITION: u64 = 2_000;

    match value {
        Value::Object(object) => {
            for keyword in ["maxLength", "maxItems"] {
                if object
                    .get(keyword)
                    .and_then(Value::as_u64)
                    .is_some_and(|bound| bound > MAX_GBNF_REPETITION)
                {
                    object.remove(keyword);
                }
            }
            for child in object.values_mut() {
                relax_llama_repetition_bounds(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                relax_llama_repetition_bounds(child);
            }
        }
        _ => {}
    }
}

pub(crate) fn project_root_object_union(object: &mut serde_json::Map<String, Value>) {
    if object.contains_key("properties") {
        return;
    }
    let Some(branches) = object
        .remove("oneOf")
        .and_then(|value| value.as_array().cloned())
    else {
        return;
    };
    if branches.is_empty()
        || branches.iter().any(|branch| {
            branch.get("type").and_then(Value::as_str) != Some("object")
                || !branch.get("properties").is_some_and(Value::is_object)
        })
    {
        object.insert("oneOf".to_owned(), Value::Array(branches));
        return;
    }
    let mut properties = serde_json::Map::new();
    let mut required: Option<std::collections::BTreeSet<String>> = None;
    for branch in &branches {
        for (name, schema) in branch["properties"].as_object().expect("checked above") {
            match properties.get_mut(name) {
                None => {
                    properties.insert(name.clone(), schema.clone());
                }
                Some(existing) if existing == schema => {}
                Some(existing) => {
                    let previous = existing.take();
                    *existing = serde_json::json!({"anyOf": [previous, schema]});
                }
            }
        }
        let branch_required = branch
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>();
        required = Some(match required {
            None => branch_required,
            Some(current) => current.intersection(&branch_required).cloned().collect(),
        });
    }
    object.insert("type".to_owned(), Value::String("object".to_owned()));
    object.insert("additionalProperties".to_owned(), Value::Bool(false));
    object.insert("properties".to_owned(), Value::Object(properties));
    object.insert(
        "required".to_owned(),
        Value::Array(
            required
                .unwrap_or_default()
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
}

pub(crate) fn apply_reasoning_request(
    body: &mut Value,
    reasoning: agl_core::agent::ReasoningSelection,
) {
    match reasoning {
        agl_core::agent::ReasoningSelection::Disabled => {
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
            body["reasoning_format"] = json!("none");
        }
        agl_core::agent::ReasoningSelection::Enabled {
            max_tokens,
            effort,
            preserve,
        } => {
            let mut kwargs = json!({
                "enable_thinking": true,
                "preserve_thinking": preserve,
            });
            if let Some(effort) = effort {
                kwargs["reasoning_effort"] = json!(match effort {
                    agl_core::agent::ReasoningEffort::Low => "low",
                    agl_core::agent::ReasoningEffort::Medium => "medium",
                    agl_core::agent::ReasoningEffort::Xhigh => "xhigh",
                });
            }
            body["chat_template_kwargs"] = kwargs;
            body["reasoning_format"] = json!("deepseek");
            body["thinking_budget_tokens"] = json!(max_tokens);
        }
    }
}

pub(crate) fn validate_reasoning_capability(
    reasoning: agl_core::agent::ReasoningSelection,
    supported: bool,
) -> Result<bool, InferenceServiceError> {
    let enabled = matches!(
        reasoning,
        agl_core::agent::ReasoningSelection::Enabled { .. }
    );
    if enabled && !supported {
        return Err(InferenceServiceError::InvalidRequest);
    }
    Ok(enabled)
}

pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn http_request(
    path: &Path,
    method: &str,
    route: &str,
    body: Option<&[u8]>,
) -> Result<HttpResponse, InferenceServiceError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| InferenceServiceError::Unavailable)?;
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            hyper_request(path, method, route, body.unwrap_or_default(), None),
        )
        .await
        .map_err(|_| InferenceServiceError::OutcomeUnknown)?
    })
}

pub(crate) async fn hyper_request(
    path: &Path,
    method: &str,
    route: &str,
    body: &[u8],
    private_attempt: Option<&str>,
) -> Result<HttpResponse, InferenceServiceError> {
    let (mut sender, connection) = hyper_connection(path).await?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(%error, "private llama-server HTTP connection failed");
        }
    });
    let mut builder = Request::builder()
        .method(
            method
                .parse::<Method>()
                .map_err(|_| InferenceServiceError::InvalidRequest)?,
        )
        .uri(route)
        .header("host", "agentlibre.internal")
        .header("content-type", "application/json");
    if let Some(attempt) = private_attempt {
        builder = builder
            .header("x-agl-protocol", "1")
            .header("x-agl-attempt-id", attempt);
    }
    let request = builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|_| InferenceServiceError::InvalidRequest)?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    let status = response.status().as_u16();
    let mut incoming = response.into_body();
    let mut body = Vec::new();
    while let Some(frame) = incoming.frame().await {
        let data = frame
            .map_err(|_| InferenceServiceError::OutcomeUnknown)?
            .into_data()
            .map_err(|_| InferenceServiceError::InvalidResult)?;
        if body.len().saturating_add(data.len()) > MAX_RESPONSE_BYTES {
            return Err(InferenceServiceError::InvalidResult);
        }
        body.extend_from_slice(&data);
    }
    Ok(HttpResponse { status, body })
}

pub(crate) async fn hyper_connection(
    path: &Path,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        hyper::client::conn::http1::Connection<TokioIo<tokio::net::UnixStream>, Full<Bytes>>,
    ),
    InferenceServiceError,
> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|_| InferenceServiceError::Unavailable)?;
    hyper::client::conn::http1::Builder::new()
        .max_buf_size(MAX_HEADER_BYTES)
        .handshake(TokioIo::new(stream))
        .await
        .map_err(|_| InferenceServiceError::Unavailable)
}

pub(crate) struct HttpGenerationResponse {
    pub(crate) status: u16,
    pub(crate) generated: Option<Generated>,
    pub(crate) diagnostic: Option<String>,
}

pub(crate) fn generation_request(
    path: &Path,
    body: &[u8],
    cancellation: &InferenceCancellation,
    attempt: &str,
    progress: Option<InferenceProgressSink>,
    reasoning_enabled: bool,
) -> Result<HttpGenerationResponse, InferenceServiceError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| InferenceServiceError::Unavailable)?;
    runtime.block_on(generation_request_async(
        path,
        body,
        cancellation,
        attempt,
        progress,
        reasoning_enabled,
    ))
}

pub(crate) async fn generation_request_async(
    path: &Path,
    body: &[u8],
    cancellation: &InferenceCancellation,
    attempt: &str,
    progress: Option<InferenceProgressSink>,
    reasoning_enabled: bool,
) -> Result<HttpGenerationResponse, InferenceServiceError> {
    let (mut sender, connection) = hyper_connection(path).await?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(%error, "private llama-server generation connection failed");
        }
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/agl/v1/generate")
        .header("host", "agentlibre.internal")
        .header("content-type", "application/json")
        .header("x-agl-protocol", "1")
        .header("x-agl-attempt-id", attempt)
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|_| InferenceServiceError::InvalidRequest)?;
    let response = sender.send_request(request);
    tokio::pin!(response);
    let mut deadline = tokio::time::Instant::now() + GENERATION_RESPONSE_TIMEOUT;
    let mut cancellation_requested = false;
    let response = loop {
        tokio::select! {
            result = &mut response => {
                break result.map_err(|error| {
                    tracing::warn!(attempt_id=%attempt, %error, "private llama-server rejected the HTTP exchange");
                    InferenceServiceError::OutcomeUnknown
                })?;
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if !cancellation_requested && cancellation.is_cancelled() {
                    tracing::warn!(attempt_id=%attempt, "sending cancellation to private llama-server before response headers");
                    request_cancel_async(path, attempt).await?;
                    cancellation_requested = true;
                    deadline = tokio::time::Instant::now() + CANCELLATION_CONFIRMATION_TIMEOUT;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(InferenceServiceError::OutcomeUnknown);
                }
            }
        }
    };
    let status = response.status().as_u16();
    let mut incoming = response.into_body();
    if status != 200 {
        let mut body = Vec::new();
        while let Some(frame) = incoming.frame().await {
            let data = frame
                .map_err(|_| InferenceServiceError::OutcomeUnknown)?
                .into_data()
                .map_err(|_| InferenceServiceError::InvalidResult)?;
            if body.len().saturating_add(data.len()) > MAX_HEADER_BYTES {
                return Err(InferenceServiceError::InvalidResult);
            }
            body.extend_from_slice(&data);
        }
        return Ok(HttpGenerationResponse {
            status,
            generated: None,
            diagnostic: Some(bounded_backend_diagnostic(&body)),
        });
    }

    let mut decoder = GenerationStreamDecoder::new(attempt, progress, reasoning_enabled);
    deadline = tokio::time::Instant::now() + GENERATION_RESPONSE_TIMEOUT;
    let mut received = 0_usize;
    loop {
        if !cancellation_requested && cancellation.is_cancelled() {
            tracing::warn!(attempt_id=%attempt, "sending cancellation to private llama-server during response stream");
            request_cancel_async(path, attempt).await?;
            cancellation_requested = true;
            deadline = tokio::time::Instant::now() + CANCELLATION_CONFIRMATION_TIMEOUT;
        }
        let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame =
            tokio::time::timeout(wait.min(Duration::from_millis(50)), incoming.frame()).await;
        match frame {
            Ok(Some(Ok(frame))) => {
                let data = frame
                    .into_data()
                    .map_err(|_| InferenceServiceError::InvalidResult)?;
                received = received.saturating_add(data.len());
                if received > MAX_RESPONSE_BYTES {
                    return Err(InferenceServiceError::InvalidResult);
                }
                decoder.feed(&data)?;
            }
            Ok(Some(Err(error))) => {
                tracing::warn!(attempt_id=%attempt, %error, "private llama-server response stream failed");
                return Err(InferenceServiceError::OutcomeUnknown);
            }
            Ok(None) => break,
            Err(_) if tokio::time::Instant::now() >= deadline => {
                return Err(InferenceServiceError::OutcomeUnknown);
            }
            Err(_) => continue,
        }
    }
    Ok(HttpGenerationResponse {
        status,
        generated: Some(decoder.finish(cancellation_requested)?),
        diagnostic: None,
    })
}

pub(crate) fn bounded_backend_diagnostic(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(512)
        .collect::<String>()
}

pub(crate) async fn request_cancel_async(
    path: &Path,
    attempt: &str,
) -> Result<(), InferenceServiceError> {
    let body = serde_json::to_vec(&json!({
        "attempt_id": attempt,
        "action": "cancel"
    }))
    .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        hyper_request(path, "POST", "/agl/v1/control", &body, None),
    )
    .await
    .map_err(|_| InferenceServiceError::OutcomeUnknown)?
    .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    if response.status != 200 {
        return Err(InferenceServiceError::OutcomeUnknown);
    }
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| InferenceServiceError::OutcomeUnknown)?;
    if value.get("schema").and_then(Value::as_str) != Some("agentlibre.llama-cancel/v1")
        || value.get("attempt_id").and_then(Value::as_str) != Some(attempt)
        || value.get("acknowledged").and_then(Value::as_bool) != Some(true)
    {
        return Err(InferenceServiceError::OutcomeUnknown);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Readiness {
    pub(crate) schema: String,
    pub(crate) plan_digest: String,
    pub(crate) reservation_id: String,
    pub(crate) engine_generation: String,
    pub(crate) context_tokens: u32,
    pub(crate) batch_size: u32,
    pub(crate) ubatch_size: u32,
    pub(crate) slot_count: u32,
    pub(crate) reasoning_supported: bool,
    pub(crate) speculative: ReadinessSpeculative,
    pub(crate) memory: Vec<MemoryAllocation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadinessSpeculative {
    pub(crate) enabled: bool,
    pub(crate) kind: String,
    pub(crate) max_draft_tokens: u32,
    #[serde(rename = "min_draft_tokens")]
    pub(crate) _min_draft_tokens: u32,
    #[serde(rename = "p_min_millionths")]
    pub(crate) _p_min_millionths: u32,
    pub(crate) gpu_layers: i32,
    pub(crate) key_cache_type: String,
    pub(crate) value_cache_type: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryAllocation {
    pub(crate) pool: String,
    pub(crate) device: String,
    pub(crate) model_bytes: u64,
    pub(crate) context_bytes: u64,
    pub(crate) compute_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObservedAllocation {
    pub(crate) host: u64,
    pub(crate) device: u64,
    pub(crate) shared: u64,
}

pub(crate) fn validate_allocation(
    profile: &LlamaRuntimeProfile,
    allocations: &[MemoryAllocation],
) -> Result<(), EngineStartError> {
    if allocations.is_empty() {
        return Err(InferenceServiceError::InvalidRequest.into());
    }
    let mut host = 0_u64;
    let mut device = 0_u64;
    let mut shared = 0_u64;
    for allocation in allocations {
        let bytes = allocation
            .model_bytes
            .checked_add(allocation.context_bytes)
            .and_then(|bytes| bytes.checked_add(allocation.compute_bytes))
            .ok_or(InferenceServiceError::InvalidRequest)?;
        match allocation.pool.as_str() {
            "host" => {
                host = host
                    .checked_add(bytes)
                    .ok_or(InferenceServiceError::InvalidRequest)?
            }
            "device"
                if profile
                    .device
                    .as_ref()
                    .is_some_and(|expected| expected == &allocation.device) =>
            {
                device = device
                    .checked_add(bytes)
                    .ok_or(InferenceServiceError::InvalidRequest)?;
            }
            "shared" => {
                shared = shared
                    .checked_add(bytes)
                    .ok_or(InferenceServiceError::InvalidRequest)?;
            }
            _ => return Err(InferenceServiceError::InvalidRequest.into()),
        }
    }
    let observed = ObservedAllocation {
        host,
        device,
        shared,
    };
    if host > profile.required_host_bytes
        || device > profile.required_device_bytes
        || shared > profile.required_shared_bytes
    {
        return Err(EngineStartError::InvalidAllocation(observed));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StreamFrame {
    pub(crate) schema: String,
    pub(crate) attempt_id: String,
    pub(crate) sequence: u64,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
    #[serde(default)]
    pub(crate) raw_output: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<Value>,
    #[serde(default)]
    pub(crate) usage: Option<StreamUsage>,
    #[serde(default)]
    pub(crate) prefill: Option<Value>,
    #[serde(default)]
    pub(crate) timings: Option<StreamTimings>,
    #[serde(default)]
    pub(crate) error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StreamUsage {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StreamTimings {
    pub(crate) cache_n: u64,
    pub(crate) prompt_n: u64,
    pub(crate) prompt_ms: f64,
    pub(crate) prompt_per_token_ms: f64,
    pub(crate) prompt_per_second: f64,
    pub(crate) predicted_n: u64,
    pub(crate) predicted_ms: f64,
    pub(crate) predicted_per_token_ms: f64,
    pub(crate) predicted_per_second: f64,
    #[serde(default)]
    pub(crate) draft_n: u64,
    #[serde(default)]
    pub(crate) draft_n_accepted: u64,
}

impl StreamTimings {
    pub(crate) fn validate(&self, usage: &StreamUsage) -> Result<(), InferenceServiceError> {
        let finite_non_negative = |value: f64| value.is_finite() && value >= 0.0;
        if self.cache_n.checked_add(self.prompt_n) != Some(usage.prompt_tokens)
            || self.predicted_n != usage.completion_tokens
            || self.draft_n_accepted > self.draft_n
            || !finite_non_negative(self.prompt_ms)
            || !finite_non_negative(self.prompt_per_token_ms)
            || !finite_non_negative(self.prompt_per_second)
            || !finite_non_negative(self.predicted_ms)
            || !finite_non_negative(self.predicted_per_token_ms)
            || !finite_non_negative(self.predicted_per_second)
        {
            return Err(InferenceServiceError::InvalidResult);
        }
        Ok(())
    }
}
