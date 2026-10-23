//! The Strata backend (AGL-030, D1, D2): an already running Strata server
//! (Qwen3.8-Flash-Next) over its OpenAI-compatible API, with no constrained
//! decoding. The flat prompt is assembled client-side; one generation per
//! phase.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{Value as Json, json};

use super::{
    Backend, Generated, Message, Prompt, RUNTIME_PREFIX, Request, Stop, ToolDoc, kept_thinking,
};
use crate::action::Value;
use crate::error::Error;
use crate::format::Format;
use crate::function::Sampling;
use crate::interrupt;

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
            url: self.url.clone(),
            detail: error.to_string(),
        }
    }

    fn bad_reply(&self, detail: impl Into<String>) -> Error {
        Error::ServerReply {
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
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Generated, Error> {
        let mut body = json!({
            "model": MODEL,
            // The flat prompt as a single user message: Strata has no
            // /apply-template, and the model's template wraps it once
            // (AGL-030, D2, D7).
            "messages": [{"role": "user", "content": request.prompt}],
            "stream": true,
            "max_tokens": request.max_tokens,
        });
        if !request.stop.is_empty() {
            body["stop"] = json!(request.stop);
        }
        add_sampling(&mut body, request.sampling);
        let response = self.send("/v1/chat/completions", Some(&body))?;
        let mut text = String::new();
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
            let step = sse_line(data).map_err(|detail| self.bad_reply(detail))?;
            match step {
                Sse::Piece(piece) => {
                    if !piece.is_empty() {
                        on_text(&piece);
                        text.push_str(&piece);
                    }
                }
                Sse::End(stop) => return Ok(Generated { text, stop }),
                Sse::Error(message) => return Err(self.bad_reply(message)),
                // The final marker without a final chunk (AGL-030, D7).
                Sse::Done => return Err(self.bad_reply("the stream ended")),
            }
        }
        Err(self.bad_reply("the stream ended without a final chunk"))
    }

    /// An estimate: the server has no /tokenize (AGL-030, D2).
    fn count_tokens(&self, text: &str) -> Result<usize, Error> {
        Ok(estimate_tokens(text))
    }

    fn context_size(&self) -> Result<usize, Error> {
        let reply = self.json("/props", None)?;
        // The context size of the loaded model, under the key the live
        // server reports; the fallback is the configured maximum
        // (AGL-030, D2).
        for key in ["context", "n_ctx", "max_context", "max_tokens", "ctx"] {
            if let Some(n) = reply.get(key).and_then(Json::as_u64) {
                return Ok(n as usize);
            }
        }
        Ok(DEFAULT_CONTEXT)
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

/// The estimate of the tokens of `text` (AGL-030, D2): `ceil(chars / 3.5)`.
fn estimate_tokens(text: &str) -> usize {
    let chars = text.chars().count() as f64;
    (chars / CHAR_DENOM).ceil() as usize
}

/// One parsed line of the SSE stream (AGL-030, D7).
enum Sse {
    /// A piece of the text, the stream is not over.
    Piece(String),
    /// The end of the stream, with its stop.
    End(Stop),
    /// A server error message, in the OpenAI dialect.
    Error(String),
    /// The final `[DONE]` marker.
    Done,
}

/// Parses one `data:` line of the SSE stream (AGL-030, D7). The model's own
/// end and a stop-string hit are not distinguishable on the OpenAI dialect;
/// both are the end.
fn sse_line(data: &str) -> Result<Sse, String> {
    if data == "[DONE]" {
        return Ok(Sse::Done);
    }
    let chunk: Json =
        serde_json::from_str(data).map_err(|error| format!("bad chunk: {error}"))?;
    if let Some(message) = chunk["error"]["message"].as_str() {
        return Ok(Sse::Error(message.to_owned()));
    }
    let choice = &chunk["choices"][0];
    let piece = choice["delta"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if let Some(reason) = choice["finish_reason"].as_str() {
        let stop = match reason {
            "length" => Stop::Limit,
            _ => Stop::End,
        };
        return Ok(Sse::End(stop));
    }
    Ok(Sse::Piece(piece))
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
    for (param, value) in args {
        out.push_str(&call.param_before);
        out.push_str(param);
        out.push_str(&call.param_between);
        out.push_str(&value_string(value));
        out.push_str(&call.param_after);
    }
    out.push_str(&call.end);
}

/// A parameter value as the model writes it.
fn value_string(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        Value::Int(n) => n.to_string(),
    }
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

    /// The stream the server sends for `generate`, as `data:` payloads
    /// (AGL-030, D7): the same loop the backend runs over the SSE lines.
    fn stream(lines: &[&str]) -> Result<crate::backend::Generated, String> {
        let mut text = String::new();
        for line in lines {
            match sse_line(line)? {
                Sse::Piece(piece) => {
                    if !piece.is_empty() {
                        text.push_str(&piece);
                    }
                }
                Sse::End(stop) => {
                    return Ok(crate::backend::Generated { text, stop })
                }
                Sse::Error(message) => return Err(message),
                Sse::Done => return Err("the stream ended".into()),
            }
        }
        Err("the stream ended without a final chunk".into())
    }

    /// A normal stream: the pieces, then the final chunk with a
    /// `finish_reason` (AGL-030, D7).
    #[test]
    fn the_sse_stream_yields_the_text_and_the_stop() {
        let generated = stream(&[
            r#"{"choices":[{"delta":{"content":"He"}}]}"#,
            r#"{"choices":[{"delta":{"content":"llo"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])
        .unwrap();
        assert_eq!(generated.text, "Hello");
        assert_eq!(generated.stop, Stop::End);
    }

    /// The stream ends without a final chunk: the action was cut off
    /// (AGL-030, D7), the error the backend reports.
    #[test]
    fn a_stream_that_ends_mid_action_is_an_error() {
        assert_eq!(
            stream(&[
                r#"{"choices":[{"delta":{"content":"<function="}}]}"#,
                r#"{"choices":[{"delta":{"content":"bash"}}]}"#,
            ])
            .err()
            .as_deref(),
            Some("the stream ended without a final chunk"),
        );
    }

    /// `length` is the limit (D18); an error event carries the server's
    /// message; `[DONE]` without a final chunk is an error (AGL-030, D7).
    #[test]
    fn the_sse_stream_maps_length_and_errors() {
        let generated = stream(&[
            r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ])
        .unwrap();
        assert_eq!(generated.stop, Stop::Limit);
        assert_eq!(
            stream(&[r#"{"error":{"message":"boom"}}"#]).err().as_deref(),
            Some("boom"),
        );
        assert!(matches!(sse_line("[DONE]").unwrap(), Sse::Done));
    }
}
