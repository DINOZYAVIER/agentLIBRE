//! The Strata backend (AGL-030, D1, D9): an already running Strata server
//! (Qwen3.8-Flash-Next) over its OpenAI-compatible API, with no constrained
//! decoding. A step is one structured generation: the server renders the
//! model's template and splits the thinking and the calls from the text;
//! the flat prompt is the client-side token estimate and the compaction.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{Value as Json, json};

use super::llama::{messages_json, tool_json};
use super::{
    Prefill, PromptProgress,
    Backend, Generated, Message, Prompt, RUNTIME_PREFIX, ReasoningLevel, Request, Stop,
    StructuredCall, ToolDoc, kept_thinking,
};
use crate::action::Value;
use crate::error::Error;
use crate::format::Format;
use crate::function::Sampling;
use crate::interrupt;
use crate::settings::ServerKind;

/// How long connecting may take (D97).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The model name the OpenAI-dialect request carries; Strata has one model
/// loaded (AGL-030, D2).
const MODEL: &str = "strata";
/// The context size when `/props` does not say (AGL-030, D2): the
/// server's configured maximum.
const DEFAULT_CONTEXT: usize = 32768;
/// The estimate of the characters of one token, Qwen-class (AGL-030, D2):
/// the tokens of `text` are `ceil(chars / 3.5)`.
const CHAR_DENOM: f64 = 3.5;

pub struct Strata {
    url: String,
    agent: ureq::Agent,
}

impl Strata {
    /// A client for the server at `url`, such as `http://127.0.0.1:18088`.
    pub fn new(url: &str) -> Strata {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .build()
            .into();
        Strata {
            url: url.trim_end_matches('/').into(),
            agent,
        }
    }

    fn unreachable(&self, error: impl std::fmt::Display) -> Error {
        Error::ServerUnreachable {
            kind: ServerKind::Strata,
            url: self.url.clone(),
            detail: error.to_string(),
        }
    }

    fn bad_reply(&self, detail: impl Into<String>) -> Error {
        Error::ServerReply {
            kind: ServerKind::Strata,
            url: self.url.clone(),
            detail: detail.into(),
        }
    }

    /// Sends a request and returns the response if the status is 200.
    fn send(
        &self,
        path: &str,
        body: Option<&Json>,
    ) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let url = format!("{}{path}", self.url);
        let response = match body {
            Some(body) => self
                .agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
            None => self.agent.get(&url).call(),
        }
        .map_err(|error| self.unreachable(error))?;
        let status = response.status().as_u16();
        if status == 200 {
            return Ok(response);
        }
        let text = response.into_body().read_to_string().unwrap_or_default();
        // The OpenAI dialect explains errors as {"error": {"message": ...}}.
        let message = serde_json::from_str::<Json>(&text)
            .ok()
            .and_then(|reply| reply["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(text);
        Err(Error::ServerStatus {
            kind: ServerKind::Strata,
            url: self.url.clone(),
            status,
            message,
        })
    }

    /// Sends a request and parses the JSON reply.
    fn json(&self, path: &str, body: Option<&Json>) -> Result<Json, Error> {
        let text = self
            .send(path, body)?
            .into_body()
            .read_to_string()
            .map_err(|error| self.unreachable(error))?;
        serde_json::from_str(&text).map_err(|error| self.bad_reply(error.to_string()))
    }
}

impl Backend for Strata {
    fn url(&self) -> &str {
        &self.url
    }

    fn health(&self) -> Option<u16> {
        let url = format!("{}/health", self.url);
        let response = self.agent.get(&url).call().ok()?;
        Some(response.status().as_u16())
    }

    fn render(&self, prompt: &Prompt) -> Result<String, Error> {
        Ok(render(prompt))
    }

    fn generate(
        &self,
        request: &Request,
        on_thinking: &mut dyn FnMut(&str),
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Generated, Error> {
        self.generate_reading(request, None, on_thinking, on_text)
    }

    fn generate_reading(
        &self,
        request: &Request,
        mut on_progress: Option<&mut dyn FnMut(PromptProgress)>,
        on_thinking: &mut dyn FnMut(&str),
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Generated, Error> {
        let mut body = match request.structured {
            Some(prompt) => structured_body(prompt, request),
            // A flat generation (the summary of a compaction): the flat
            // prompt as a single user message (AGL-030, D2).
            None => json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": request.prompt}],
                "stream": true,
                "max_tokens": request.max_tokens,
            }),
        };
        if !request.stop.is_empty() {
            body["stop"] = json!(request.stop);
        }
        add_sampling(&mut body, request.sampling);
        if on_progress.is_some() {
            body["return_progress"] = true.into();
        }
        let response = self.send("/v1/chat/completions", Some(&body))?;
        let mut stream = Accumulator::default();
        let reader = BufReader::new(response.into_body().into_reader());
        for line in reader.lines() {
            // Ctrl-C interrupts the read (D111); dropping the response
            // closes the connection, and the server stops generating.
            if interrupt::interrupted() {
                return Err(Error::Interrupted);
            }
            let line = line.map_err(|error| {
                if interrupt::interrupted() {
                    Error::Interrupted
                } else {
                    self.unreachable(error)
                }
            })?;
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if let Some(tell) = on_progress.as_mut()
                && data.contains("\"prompt_progress\"")
                && let Some(progress) = serde_json::from_str(data)
                    .ok()
                    .as_ref()
                    .and_then(PromptProgress::of_chunk)
            {
                (*tell)(progress);
            }
            let done = stream
                .feed(data, on_thinking, on_text)
                .map_err(|detail| self.bad_reply(detail))?;
            if done {
                break;
            }
        }
        stream.finish().map_err(|detail| self.bad_reply(detail))
    }

    /// The tokens of the structured prompt as the server counts them: the
    /// system prompt, the conversation and the tools in the Anthropic
    /// dialect of the same endpoint, which the server renders as it renders
    /// the request (AGL-030, D11). A server without the endpoint gets the
    /// count of the flat text.
    fn count_prompt(&self, prompt: &Prompt, flat: &str) -> Result<usize, Error> {
        match self.json("/v1/messages/count_tokens", Some(&anthropic_body(prompt))) {
            Ok(reply) => match reply["input_tokens"].as_u64() {
                Some(n) => Ok(n as usize),
                None => self.count_tokens(flat),
            },
            Err(Error::ServerStatus { .. }) => self.count_tokens(flat),
            Err(error) => Err(error),
        }
    }

    /// The tokens of `text` as the server counts them: it renders and
    /// tokenizes `text` as one user message (`/v1/messages/count_tokens`, the
    /// few tokens of the template included, so a slight overcount); a
    /// server without that endpoint gets the estimate (AGL-030, D11).
    fn count_tokens(&self, text: &str) -> Result<usize, Error> {
        let body = json!({"model": MODEL, "messages": [{"role": "user", "content": text}]});
        match self.json("/v1/messages/count_tokens", Some(&body)) {
            Ok(reply) => Ok(reply["input_tokens"]
                .as_u64()
                .map_or_else(|| estimate_tokens(text), |n| n as usize)),
            Err(Error::ServerStatus { .. }) => Ok(estimate_tokens(text)),
            Err(error) => Err(error),
        }
    }

    fn context_size(&self) -> Result<usize, Error> {
        // The server's `/props` carries the context under
        // `default_generation_settings`, `/health` as `max_context`
        // (AGL-030, D2); the fallback is the configured maximum.
        if let Ok(reply) = self.json("/props", None)
            && let Some(n) = reply["default_generation_settings"]["n_ctx"].as_u64()
        {
            return Ok(n as usize);
        }
        if let Ok(reply) = self.json("/health", None)
            && let Some(n) = reply["max_context"].as_u64()
        {
            return Ok(n as usize);
        }
        Ok(DEFAULT_CONTEXT)
    }

    fn models(&self) -> Result<Option<String>, Error> {
        let reply = self.json("/v1/models", None)?;
        // The OpenAI dialect lists under `data`; the `id` is the model
        // name the server was started with (AGL-033, D2).
        let entry = reply["data"].as_array().and_then(|a| a.first());
        let Some(entry) = entry else {
            return Ok(None);
        };
        let name = entry["id"].as_str();
        Ok(name.filter(|s| !s.is_empty()).map(str::to_owned))
    }
}

/// Adds the sampling settings that the Function sets (D89).
fn add_sampling(body: &mut Json, sampling: &Sampling) {
    let numbers = [
        ("temperature", sampling.temperature),
        ("top_p", sampling.top_p),
        ("min_p", sampling.min_p),
        ("presence_penalty", sampling.presence_penalty),
        ("repeat_penalty", sampling.repeat_penalty),
    ];
    for (key, value) in numbers {
        if let Some(value) = value {
            body[key] = value.into();
        }
    }
    if let Some(top_k) = sampling.top_k {
        body["top_k"] = top_k.into();
    }
}

/// The estimate of the tokens of `text` (AGL-030, D2), for a server that does
/// not count them: `ceil(chars / 3.5)`.
fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count() as f64;
    (chars / CHAR_DENOM).ceil() as usize
}

/// The body of a structured step (AGL-030, D9): clean roles, the tools,
/// the reasoning effort, one generation with the exact `usage` at the end.
fn structured_body(prompt: &Prompt, request: &Request) -> Json {
    let mut body = json!({
        "model": MODEL,
        "messages": messages_json(prompt),
        "stream": true,
        "stream_options": {"include_usage": true},
        "parallel_tool_calls": false,
        "max_tokens": request.max_tokens,
    });
    if !prompt.tools.is_empty() {
        body["tools"] = prompt.tools.iter().map(|doc| tool_json(doc)).collect();
    }
    if let Some(effort) = reasoning_effort(prompt.reasoning) {
        body["reasoning_effort"] = effort.into();
    }
    body
}

/// The structured prompt in the Anthropic dialect, for the server's token
/// count (AGL-030, D11): the system prompt, the messages with their calls
/// and results as blocks, the state instruction last, the tools.
fn anthropic_body(prompt: &Prompt) -> Json {
    let kept = kept_thinking(prompt.messages, prompt.preserve_thinking);
    let mut system = String::new();
    let mut messages: Vec<Json> = Vec::new();
    for (message, keep_thinking) in prompt.messages.iter().zip(kept) {
        match message {
            Message::System(text) => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(text);
            }
            Message::User(text) => messages.push(json!({"role": "user", "content": text})),
            Message::Runtime(text) => messages
                .push(json!({"role": "user", "content": format!("{RUNTIME_PREFIX} {text}")})),
            Message::Assistant {
                thinking,
                text,
                call,
            } => {
                let mut blocks = Vec::new();
                if let Some(thinking) = thinking.as_deref().filter(|_| keep_thinking) {
                    blocks.push(json!({"type": "thinking", "thinking": thinking, "signature": ""}));
                }
                blocks.push(json!({"type": "text", "text": text.as_deref().unwrap_or_default()}));
                if let Some(call) = call {
                    let input: serde_json::Map<String, Json> = call
                        .args
                        .iter()
                        .map(|(name, value)| {
                            let value = match value {
                                Value::Text(text) => Json::from(text.as_str()),
                                Value::Int(n) => Json::from(*n),
                            };
                            (name.clone(), value)
                        })
                        .collect();
                    blocks.push(
                        json!({"type": "tool_use", "id": call.id, "name": call.name, "input": input}),
                    );
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::Tool { id, content } => messages.push(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": id, "content": content}],
            })),
        }
    }
    if let Some(instruction) = prompt.instruction {
        messages.push(json!({"role": "user", "content": instruction}));
    }
    let mut body = json!({"model": MODEL, "system": system, "messages": messages});
    if !prompt.tools.is_empty() {
        body["tools"] = prompt
            .tools
            .iter()
            .map(|doc| {
                let function = tool_json(doc)["function"].clone();
                json!({
                    "name": function["name"],
                    "description": function["description"],
                    "input_schema": function["parameters"],
                })
            })
            .collect();
    }
    body
}

/// The `reasoning_effort` of a level (AGL-013, AGL-030, D9): `Off` is
/// `none` — the server then does not think —, a level its own name; none
/// when the level is unset, so the server keeps its default.
fn reasoning_effort(level: Option<ReasoningLevel>) -> Option<&'static str> {
    level.map(|level| match level {
        ReasoningLevel::Off => "none",
        level => level.name(),
    })
}

/// What the stream of one generation brought so far (AGL-030, D9, D11):
/// the pieces of the text and of the thinking, the calls by `index`, the
/// stop and the exact `usage`.
#[derive(Default)]
struct Accumulator {
    text: String,
    thinking: String,
    calls: Vec<StructuredCall>,
    stop: Option<Stop>,
    prompt_tokens: Option<usize>,
    completion_tokens: Option<usize>,
    prefill: Option<Prefill>,
}

impl Accumulator {
    /// Takes one `data:` payload; `true` at the final `[DONE]`. The
    /// pieces of the thinking and of the text go to the reporters as they
    /// arrive. The model's own end and a stop-string hit are not
    /// distinguishable on the OpenAI dialect; both are the end.
    fn feed(
        &mut self,
        data: &str,
        on_thinking: &mut dyn FnMut(&str),
        on_text: &mut dyn FnMut(&str),
    ) -> Result<bool, String> {
        if data == "[DONE]" {
            return Ok(true);
        }
        let chunk: Json =
            serde_json::from_str(data).map_err(|error| format!("bad chunk: {error}"))?;
        if let Some(message) = chunk["error"]["message"].as_str() {
            return Err(message.to_owned());
        }
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];
        if let Some(piece) = delta["reasoning_content"].as_str().filter(|p| !p.is_empty()) {
            on_thinking(piece);
            self.thinking.push_str(piece);
        }
        if let Some(piece) = delta["content"].as_str().filter(|p| !p.is_empty()) {
            on_text(piece);
            self.text.push_str(piece);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            // A call is being written: no text comes of it (AGL-058, D7).
            on_text("");
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            while self.calls.len() <= index {
                self.calls.push(StructuredCall {
                    name: String::new(),
                    arguments: String::new(),
                });
            }
            let slot = &mut self.calls[index];
            if let Some(name) = call["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                slot.name = name.to_owned();
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                slot.arguments.push_str(arguments);
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop = Some(match reason {
                "length" => Stop::Limit,
                _ => Stop::End,
            });
        }
        if let Some(n) = chunk["usage"]["prompt_tokens"].as_u64() {
            self.prompt_tokens = Some(n as usize);
        }
        if let Some(n) = chunk["usage"]["completion_tokens"].as_u64() {
            self.completion_tokens = Some(n as usize);
        }
        if let Some(prefill) = Prefill::of_timings(&chunk["timings"]) {
            self.prefill = Some(prefill);
        }
        Ok(false)
    }

    /// The generation, when the stream reached its final chunk.
    fn finish(self) -> Result<Generated, String> {
        let stop = self
            .stop
            .ok_or_else(|| "the stream ended without a final chunk".to_owned())?;
        Ok(Generated {
            text: self.text,
            stop,
            thinking: Some(self.thinking).filter(|thinking| !thinking.is_empty()),
            tool_calls: self
                .calls
                .into_iter()
                .filter(|call| !call.name.is_empty())
                .collect(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            prefill: self.prefill,
        })
    }
}

/// The flat prompt the Strata backend sends: the system prompt, the tool
/// docs as text, the conversation in the model's format, and the state
/// instruction last (AGL-030, D2). Deterministic; the calls of the
/// conversation are rendered in the model's format.
fn render(prompt: &Prompt) -> String {
    let format = prompt.format;
    let kept = kept_thinking(prompt.messages, prompt.preserve_thinking);
    // The leading System message is the system prompt; the rest is the
    // conversation.
    let (system, rest, rest_kept) = match prompt.messages.first() {
        Some(Message::System(text)) => (
            Some(text.as_str()),
            &prompt.messages[1..],
            &kept[1..],
        ),
        _ => (None, prompt.messages, kept.as_slice()),
    };

    let mut out = String::new();
    if let Some(system) = system {
        out.push_str(system);
        out.push_str("\n\n");
    }
    if !prompt.tools.is_empty() {
        out.push_str("Tools:\n");
        for doc in prompt.tools {
            render_tool_doc(&mut out, doc);
        }
        out.push('\n');
    }
    for (message, keep_thinking) in rest.iter().zip(rest_kept) {
        render_message(&mut out, format, message, *keep_thinking);
    }
    if let Some(instruction) = prompt.instruction {
        out.push_str("User: ");
        out.push_str(instruction);
        out.push('\n');
    }
    out
}

/// One message of the conversation in the flat prompt (AGL-030, D2).
fn render_message(out: &mut String, format: &Format, message: &Message, keep_thinking: bool) {
    match message {
        Message::System(text) => {
            out.push_str("System: ");
            out.push_str(text);
            out.push_str("\n\n");
        }
        Message::User(text) => {
            out.push_str("User: ");
            out.push_str(text);
            out.push_str("\n\n");
        }
        Message::Runtime(text) => {
            // A runtime message is the human's prefix line as a user turn
            // (AGL-030, D2, AGL-009).
            out.push_str("User: ");
            out.push_str(RUNTIME_PREFIX);
            out.push(' ');
            out.push_str(text);
            out.push_str("\n\n");
        }
        Message::Assistant {
            thinking,
            text,
            call,
        } => {
            out.push_str("Assistant: ");
            if let Some(thinking) = thinking.as_deref().filter(|_| keep_thinking) {
                render_thinking(out, format, thinking);
            }
            if let Some(text) = text {
                out.push_str(text);
            }
            if let Some(call) = call {
                render_call(out, format, &call.name, &call.args);
            }
            out.push_str("\n\n");
        }
        Message::Tool { id, content } => {
            out.push_str("Tool result (");
            out.push_str(id);
            out.push_str("): ");
            out.push_str(content);
            out.push_str("\n\n");
        }
    }
}

/// The thinking block in the model's format (AGL-030, D2).
fn render_thinking(out: &mut String, format: &Format, thinking: &str) {
    if let Some(think) = &format.think {
        out.push_str(&think.open);
        out.push_str(thinking);
        out.push_str(&think.close);
    } else {
        out.push_str(thinking);
    }
}

/// A call of the conversation in the model's format (AGL-030, D2).
fn render_call(out: &mut String, format: &Format, name: &str, args: &[(String, Value)]) {
    let call = &format.call;
    out.push_str(&call.begin_before);
    out.push_str(name);
    out.push_str(&call.begin_after);
    call.write_args(out, args);
    out.push_str(&call.end);
}

/// A tool doc as a text block of the flat prompt (AGL-030, D2).
fn render_tool_doc(out: &mut String, doc: &ToolDoc) {
    out.push_str("- ");
    out.push_str(doc.name);
    out.push_str(": ");
    out.push_str(doc.description);
    out.push('\n');
    for &(name, integer, description, is_required) in doc.params {
        out.push_str("  - ");
        out.push_str(name);
        out.push_str(if integer { " (integer" } else { " (string" });
        if is_required {
            out.push_str(", required");
        }
        out.push(')');
        if let Some(description) = description {
            out.push_str(": ");
            out.push_str(description);
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Message, Prompt, Stop, ToolCall, ToolDoc};
    use crate::format::tests::qwen;

    /// The doc of the `bash` tool, as the flat prompt shows it (AGL-030, D2).
    const BASH: ToolDoc = ToolDoc {
        name: "bash",
        description: "Run a shell command.",
        params: &[
            ("command", false, None, true),
            ("timeout", true, Some("Seconds; default 60."), false),
        ],
    };

    /// The flat prompt of a fixed conversation (AGL-030, D2): the system
    /// prompt, the tool docs as text, the conversation in the model's format
    /// (a runtime message is a prefixed user turn), the instruction last.
    #[test]
    fn render_of_a_fixed_conversation_is_the_flat_prompt() {
        let format = qwen();
        let messages = [
            Message::System("You are s.".into()),
            Message::User("hello".into()),
            Message::Assistant {
                thinking: Some("think".into()),
                text: Some("I will.".into()),
                call: Some(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: vec![("command".into(), Value::Text("ls".into()))],
                }),
            },
            Message::Runtime("It was cut off.".into()),
        ];
        let prompt = Prompt {
            messages: &messages,
            instruction: Some("Answer now."),
            tools: &[&BASH],
            preserve_thinking: true,
            reasoning: None,
            format: &format,
        };
        // The expected flat text, assembled from the format's own markers,
        // so the test reads as the conversation (AGL-030, D2).
        let think = format.think.as_ref().unwrap();
        let call = &format.call;
        let call_text = format!(
            "{before}bash{after}<parameter=command>\nls\n</parameter>\n{end}",
            before = call.begin_before,
            after = call.begin_after,
            end = call.end,
        );
        let mut expected = String::from(concat!(
            "You are s.\n\nTools:\n- bash: Run a shell command.\n",
            "  - command (string, required)\n",
            "  - timeout (integer): Seconds; default 60.\n",
            "\nUser: hello\n\nAssistant: ",
        ));
        expected.push_str(&think.open);
        expected.push_str("think");
        expected.push_str(&think.close);
        expected.push_str("I will.");
        expected.push_str(&call_text);
        expected.push_str("\n\n");
        expected.push_str("User: [agentLIBRE runtime] It was cut off.\n\n");
        expected.push_str("User: Answer now.\n");
        assert_eq!(render(&prompt), expected);
    }

    /// The stream of one generation, as `data:` payloads: the backend's own
    /// accumulator over the SSE lines, with what reached the reporters
    /// (AGL-030, D9).
    fn stream(lines: &[&str]) -> (Result<Generated, String>, String, String) {
        let mut thinking = String::new();
        let mut text = String::new();
        let mut acc = Accumulator::default();
        for line in lines {
            match acc.feed(
                line,
                &mut |piece| thinking.push_str(piece),
                &mut |piece| text.push_str(piece),
            ) {
                Ok(true) => break,
                Ok(false) => {}
                Err(message) => return (Err(message), thinking, text),
            }
        }
        (acc.finish(), thinking, text)
    }

    /// A thinking and a text stream (an answer): the pieces reach the two
    /// reporters, the final chunk carries the stop and the exact usage
    /// (AGL-030, D9, D11).
    #[test]
    fn the_stream_of_an_answer_yields_the_thinking_the_text_and_the_usage() {
        let (generated, thinking, text) = stream(&[
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"Hmm"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"."}}]}"#,
            r#"{"choices":[{"delta":{"content":"He"}}]}"#,
            r#"{"choices":[{"delta":{"content":"llo"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":312,"completion_tokens":9}}"#,
            "[DONE]",
        ]);
        let generated = generated.unwrap();
        assert_eq!(generated.text, "Hello");
        assert_eq!(generated.thinking.as_deref(), Some("Hmm."));
        assert_eq!(generated.stop, Stop::End);
        assert!(generated.tool_calls.is_empty());
        assert_eq!(generated.prompt_tokens, Some(312));
        assert_eq!(generated.completion_tokens, Some(9));
        assert_eq!((thinking.as_str(), text.as_str()), ("Hmm.", "Hello"));
    }

    /// The pieces of a call arrive by `index`: the name once, the
    /// arguments in pieces; a usage chunk after the finish is taken too
    /// (AGL-030, D9, D11).
    #[test]
    fn the_stream_of_a_call_accumulates_the_arguments_by_index() {
        let (generated, _, text) = stream(&[
            r#"{"choices":[{"delta":{"content":"Let me look."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"bash","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":7}}"#,
            "[DONE]",
        ]);
        let generated = generated.unwrap();
        assert_eq!(text, "Let me look.");
        assert_eq!(generated.stop, Stop::End);
        assert_eq!(
            generated.tool_calls,
            [StructuredCall {
                name: "bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            }]
        );
        assert_eq!(generated.prompt_tokens, Some(40));
    }

    /// `length` is the limit (D18), also in the middle of the arguments; an
    /// error event carries the server's message; a stream without its
    /// final chunk is an error (AGL-030, D9).
    #[test]
    fn the_stream_maps_length_errors_and_a_missing_end() {
        let (generated, _, _) = stream(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"bash","arguments":"{\"comm"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ]);
        let generated = generated.unwrap();
        assert_eq!(generated.stop, Stop::Limit);
        assert_eq!(generated.tool_calls[0].arguments, "{\"comm");
        assert_eq!(
            stream(&[r#"{"error":{"message":"boom"}}"#]).0.err().as_deref(),
            Some("boom"),
        );
        assert_eq!(
            stream(&[r#"{"choices":[{"delta":{"content":"x"}}]}"#])
                .0
                .err()
                .as_deref(),
            Some("the stream ended without a final chunk"),
        );
    }

    /// The structured body (AGL-030, D9): the clean roles, the tools, the
    /// reasoning effort, the usage option and one call at a time.
    #[test]
    fn the_structured_body_carries_the_conversation_the_tools_and_the_effort() {
        let format = qwen();
        let messages = [
            Message::System("You are s.".into()),
            Message::User("hello".into()),
            Message::Assistant {
                thinking: Some("think".into()),
                text: Some("I will.".into()),
                call: Some(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: vec![("command".into(), Value::Text("ls".into()))],
                }),
            },
            Message::Tool {
                id: "c1".into(),
                content: "a.txt".into(),
            },
            Message::Runtime("Note.".into()),
        ];
        let prompt = Prompt {
            messages: &messages,
            instruction: Some("Answer now."),
            tools: &[&BASH],
            preserve_thinking: true,
            reasoning: Some(ReasoningLevel::Xhigh),
            format: &format,
        };
        let sampling = Sampling::default();
        let request = Request {
            prompt: "",
            grammar: None,
            stop: &[],
            max_tokens: 700,
            sampling: &sampling,
            media: &[],
            structured: Some(&prompt),
        };
        let body = structured_body(&prompt, &request);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["max_tokens"], 700);
        assert_eq!(body["reasoning_effort"], "xhigh");
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user", "user"]);
        let assistant = &body["messages"][2];
        assert_eq!(assistant["reasoning_content"], "think");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");
        // The level `Off` is `none`, an unset level omits the field, a
        // state without tools sends no `tools`.
        let off = Prompt {
            reasoning: Some(ReasoningLevel::Off),
            tools: &[],
            ..prompt
        };
        let body = structured_body(&off, &request);
        assert_eq!(body["reasoning_effort"], "none");
        assert!(body.get("tools").is_none());
        let unset = Prompt {
            reasoning: None,
            ..off
        };
        assert!(structured_body(&unset, &request).get("reasoning_effort").is_none());
    }

    /// A server that answers one request with `status` and `body`.
    fn one_reply(status: u16, body: &'static str) -> String {
        replies(status, body, 1)
    }

    /// A server that answers `times` requests with `status` and `body`.
    fn replies(status: u16, body: &'static str, times: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for _ in 0..times {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0);
            let mut request = vec![0u8; length];
            stream.read_exact(&mut request).unwrap();
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        url
    }

    /// The tokens are the server's own count; a server without the endpoint
    /// gets the estimate (AGL-030, D11).
    #[test]
    fn count_tokens_asks_the_server_and_falls_back_to_the_estimate() {
        let strata = Strata::new(&one_reply(200, r#"{"input_tokens": 1234}"#));
        assert_eq!(strata.count_tokens("hello").unwrap(), 1234);
        let strata = Strata::new(&one_reply(404, r#"{"error":{"message":"not found"}}"#));
        assert_eq!(strata.count_tokens("0123456789").unwrap(), 3);
    }

    /// The Anthropic body of the structured prompt (AGL-030, D11): the
    /// system prompt apart, a call and its result as blocks, the tools as
    /// `input_schema`; the server's answer is the count.
    #[test]
    fn count_prompt_sends_the_structured_prompt_in_the_anthropic_dialect() {
        let format = qwen();
        let messages = [
            Message::System("You are s.".into()),
            Message::User("hello".into()),
            Message::Assistant {
                thinking: Some("think".into()),
                text: None,
                call: Some(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: vec![("command".into(), Value::Text("ls".into()))],
                }),
            },
            Message::Tool {
                id: "c1".into(),
                content: "a.txt".into(),
            },
        ];
        let prompt = Prompt {
            messages: &messages,
            instruction: Some("Go."),
            tools: &[&BASH],
            preserve_thinking: true,
            reasoning: None,
            format: &format,
        };
        let body = anthropic_body(&prompt);
        assert_eq!(body["system"], "You are s.");
        let blocks = &body["messages"][1]["content"];
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[2]["type"], "tool_use");
        assert_eq!(blocks[2]["input"]["command"], "ls");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "c1");
        assert_eq!(body["messages"][3]["content"], "Go.");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["input_schema"]["required"][0], "command");
        let strata = Strata::new(&one_reply(200, r#"{"input_tokens": 4321}"#));
        assert_eq!(strata.count_prompt(&prompt, "flat").unwrap(), 4321);
        // A server without the endpoint: the flat text's estimate.
        let strata = Strata::new(&replies(404, r#"{"error":{"message":"not found"}}"#, 2));
        assert_eq!(strata.count_prompt(&prompt, "0123456789").unwrap(), 3);
    }
}
