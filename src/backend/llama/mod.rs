//! The llama.cpp backend: an already running llama-server over HTTP,
//! upstream API only (D29, D37, D52, D97).

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{Map, Value as Json, json};

use super::{
    Prefill, PromptProgress,
    Backend, Generated, Message, Prompt, RUNTIME_PREFIX, ReasoningLevel, Request, Stop, ToolDoc,
    kept_thinking,
};
use crate::action::Value;
use crate::error::Error;
use crate::function::Sampling;
use crate::interrupt;
use crate::settings::ServerKind;

/// How long connecting may take (D97).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Llama {
    url: String,
    agent: ureq::Agent,
}

impl Llama {
    /// A client for the server at `url`, such as `http://127.0.0.1:18080`.
    pub fn new(url: &str) -> Llama {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .build()
            .into();
        Llama {
            url: url.trim_end_matches('/').into(),
            agent,
        }
    }

    fn unreachable(&self, error: impl std::fmt::Display) -> Error {
        Error::ServerUnreachable {
            kind: ServerKind::Llama,
            url: self.url.clone(),
            detail: error.to_string(),
        }
    }

    fn bad_reply(&self, detail: impl Into<String>) -> Error {
        Error::ServerReply {
            kind: ServerKind::Llama,
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
        // llama-server explains errors as {"error": {"message": ...}}.
        let message = serde_json::from_str::<Json>(&text)
            .ok()
            .and_then(|reply| reply["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(text);
        Err(Error::ServerStatus {
            kind: ServerKind::Llama,
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

impl Backend for Llama {
    fn url(&self) -> &str {
        &self.url
    }

    fn health(&self) -> Option<u16> {
        let url = format!("{}/health", self.url);
        let response = self.agent.get(&url).call().ok()?;
        Some(response.status().as_u16())
    }

    fn render(&self, prompt: &Prompt) -> Result<String, Error> {
        let body = apply_template_body(prompt);
        let reply = self.json("/apply-template", Some(&body))?;
        reply["prompt"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| self.bad_reply("no `prompt` in the /apply-template reply"))
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
        _on_thinking: &mut dyn FnMut(&str),
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Generated, Error> {
        let mut body = json!({
            "prompt": request.prompt,
            "n_predict": request.max_tokens,
            "stop": request.stop,
            "stream": true,
            "cache_prompt": true,
        });
        if on_progress.is_some() {
            body["return_progress"] = true.into();
        }
        if !request.media.is_empty() {
            body["prompt"] = json!({
                "prompt_string": request.prompt,
                "multimodal_data": request.media,
            });
        }
        if let Some(grammar) = request.grammar {
            body["grammar"] = grammar.into();
        }
        add_sampling(&mut body, request.sampling);
        let response = self.send("/completion", Some(&body))?;
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
            let chunk: Json =
                serde_json::from_str(data).map_err(|error| self.bad_reply(error.to_string()))?;
            if let Some(message) = chunk["error"]["message"].as_str() {
                return Err(self.bad_reply(message));
            }
            if let (Some(tell), Some(progress)) =
                (on_progress.as_mut(), PromptProgress::of_chunk(&chunk))
            {
                (*tell)(progress);
            }
            let piece = chunk["content"].as_str().unwrap_or_default();
            if !piece.is_empty() {
                on_text(piece);
                text.push_str(piece);
            }
            if chunk["stop"] == true {
                let stop = match chunk["stop_type"].as_str() {
                    Some("limit") => Stop::Limit,
                    Some("word") => {
                        Stop::Word(chunk["stopping_word"].as_str().unwrap_or_default().into())
                    }
                    _ => Stop::End,
                };
                let mut generated = Generated::plain(text, stop);
                generated.prefill = Prefill::of_timings(&chunk["timings"]);
                return Ok(generated);
            }
        }
        Err(self.bad_reply("the stream ended without a final chunk"))
    }

    fn count_tokens(&self, text: &str) -> Result<usize, Error> {
        let reply = self.json("/tokenize", Some(&json!({ "content": text })))?;
        reply["tokens"]
            .as_array()
            .map(Vec::len)
            .ok_or_else(|| self.bad_reply("no `tokens` in the /tokenize reply"))
    }

    fn context_size(&self) -> Result<usize, Error> {
        let reply = self.json("/props", None)?;
        reply["default_generation_settings"]["n_ctx"]
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| self.bad_reply("no `n_ctx` in the /props reply"))
    }

    fn models(&self) -> Result<Option<String>, Error> {
        let reply = self.json("/v1/models", None)?;
        // The llama-server lists under `models` (its `model`, else its
        // `name`); the OpenAI dialect lists under `data` (its `id`).
        let entry = reply["models"]
            .as_array()
            .and_then(|a| a.first())
            .or_else(|| reply["data"].as_array().and_then(|a| a.first()));
        let Some(entry) = entry else {
            return Ok(None);
        };
        let name = entry["model"]
            .as_str()
            .or_else(|| entry["name"].as_str())
            .or_else(|| entry["id"].as_str());
        Ok(name.filter(|s| !s.is_empty()).map(str::to_owned))
    }

    fn media_marker(&self) -> Result<String, Error> {
        let reply = self.json("/props", None)?;
        let vision = reply["modalities"]["vision"].as_bool().unwrap_or(false);
        match reply["media_marker"].as_str() {
            Some(marker) if vision => Ok(marker.to_owned()),
            _ => Err(Error::NoMultimodal),
        }
    }

    fn hears_audio(&self) -> Result<bool, Error> {
        let reply = self.json("/props", None)?;
        Ok(reply["modalities"]["audio"].as_bool().unwrap_or(false))
    }
}

/// The body of a render: the messages and tools, plus the reasoning
/// level's `chat_template_kwargs` when it is set (AGL-013).
fn apply_template_body(prompt: &Prompt) -> Json {
    let mut body = json!({
        "messages": messages_json(prompt),
        "tools": prompt.tools.iter().map(|doc| tool_json(doc)).collect::<Vec<_>>(),
    });
    if let Some(kwargs) = reasoning_kwargs(prompt.reasoning) {
        body["chat_template_kwargs"] = kwargs;
    }
    body
}

/// The `chat_template_kwargs` of a reasoning level (AGL-013): `Off`
/// silences the thinking, a level sets the template's
/// `reasoning_effort`. None when the level is unset, so the template
/// keeps its default.
fn reasoning_kwargs(level: Option<ReasoningLevel>) -> Option<Json> {
    match level {
        None => None,
        Some(ReasoningLevel::Off) => Some(json!({"enable_thinking": false})),
        Some(level) => Some(json!({"reasoning_effort": level.name()})),
    }
}

/// The messages in the OpenAI-style form of `/apply-template`, then the
/// state instruction as a last user message (D19).
pub(crate) fn messages_json(prompt: &Prompt) -> Vec<Json> {
    let kept = kept_thinking(prompt.messages, prompt.preserve_thinking);
    let mut out: Vec<Json> = prompt
        .messages
        .iter()
        .zip(kept)
        .map(|(message, keep_thinking)| match message {
            Message::System(text) => json!({"role": "system", "content": text}),
            Message::User(text) => json!({"role": "user", "content": text}),
            Message::Runtime(text) => {
                json!({"role": "user", "content": format!("{RUNTIME_PREFIX} {text}")})
            }
            Message::Assistant {
                thinking,
                text,
                call,
            } => {
                let mut json = json!({
                    "role": "assistant",
                    "content": text.as_deref().unwrap_or_default(),
                });
                if let Some(thinking) = thinking.as_deref().filter(|_| keep_thinking) {
                    json["reasoning_content"] = thinking.into();
                }
                if let Some(call) = call {
                    let args: Map<String, Json> = call
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
                    json["tool_calls"] = json!([{
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": Json::Object(args).to_string()},
                    }]);
                }
                json
            }
            Message::Tool { id, content } => {
                json!({"role": "tool", "tool_call_id": id, "content": content})
            }
        })
        .collect();
    if let Some(instruction) = prompt.instruction {
        out.push(json!({"role": "user", "content": instruction}));
    }
    out
}

/// A tool as a JSON-schema function for the template (D95).
pub(crate) fn tool_json(doc: &ToolDoc) -> Json {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for &(name, integer, description, is_required) in doc.params {
        let mut property = json!({"type": if integer { "integer" } else { "string" }});
        if let Some(description) = description {
            property["description"] = description.into();
        }
        properties.insert(name.into(), property);
        if is_required {
            required.push(name);
        }
    }
    json!({
        "type": "function",
        "function": {
            "name": doc.name,
            "description": doc.description,
            "parameters": {"type": "object", "properties": properties, "required": required},
        },
    })
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

#[cfg(test)]
mod server_test;
#[cfg(test)]
mod tests;
