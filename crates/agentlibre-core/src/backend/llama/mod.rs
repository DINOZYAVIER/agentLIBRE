//! The llama.cpp backend: an already running llama-server over HTTP,
//! upstream API only (D29, D37, D52, D97).

use std::io::{BufRead, BufReader};

use serde_json::{Map, Value as Json, json};

use super::http::Http;
use super::{
    Prefill, PromptProgress, add_sampling,
    Backend, Generated, Message, Prompt, RUNTIME_PREFIX, ReasoningLevel, Request, Stop,
    kept_thinking,
};
use crate::action::{Kind, Value};
use crate::error::Error;
use crate::backend::error::Error as BackendError;
use crate::tools::Spec;
use crate::settings::ServerKind;

pub struct Llama {
    http: Http,
}

impl Llama {
    /// A client for the server at `url`, such as `http://127.0.0.1:18080`.
    pub fn new(url: &str) -> Llama {
        Llama {
            http: Http::new(ServerKind::Llama, url),
        }
    }

    /// A client that trusts the CA in the file `ca`, when one is named
    /// (AGL-058, D12).
    pub fn with_ca(url: &str, ca: Option<&str>) -> Result<Llama, Error> {
        let Some(ca) = ca else {
            return Ok(Llama::new(url));
        };
        Ok(Llama {
            http: Http::with_ca(ServerKind::Llama, url, ca)?,
        })
    }

    /// The same client, its certificate failures naming `key` instead of
    /// `server.llama.private_ca`: the probe of a whisper server (AGL-121,
    /// D4).
    pub fn naming_ca(self, key: &'static str) -> Llama {
        Llama {
            http: self.http.naming_ca(key),
        }
    }

    /// Sends a request and parses the JSON reply.
    /// The text of `wav` as the model hears it (AGL-051): the language is
    /// the one spoken, and the human checks it in the input line. A recording
    /// with no sound is not sent: a model makes something up for it.
    pub fn transcribe(&self, wav: &[u8]) -> Result<String, Error> {
        if crate::media::wav_is_silent(wav) {
            return Ok(String::new());
        }
        let reply = self.http.json("/v1/chat/completions", Some(&transcribe_body(wav)))?;
        reply["choices"][0]["message"]["content"]
            .as_str()
            .map(|text| before_its_marks(text).to_owned())
            .ok_or_else(|| self.http.bad_reply("no `content` in the reply"))
    }
}

impl Backend for Llama {
    fn url(&self) -> &str {
        &self.http.url
    }

    fn health(&self) -> Option<u16> {
        let url = format!("{}/health", self.http.url);
        let response = self.http.agent.get(&url).call().ok()?;
        Some(response.status().as_u16())
    }

    fn render(&self, prompt: &Prompt) -> Result<String, Error> {
        let body = apply_template_body(prompt);
        let reply = self.http.json("/apply-template", Some(&body))?;
        reply["prompt"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| self.http.bad_reply("no `prompt` in the /apply-template reply"))
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
        let response = self.http.send("/completion", Some(&body))?;
        let mut text = String::new();
        let reader = BufReader::new(response.into_body().into_reader());
        for line in reader.lines() {
            // Ctrl-C interrupts the read (D111); dropping the response
            // closes the connection, and the server stops generating.
            if crate::stop::requested() {
                return Err(Error::Common(agentlibre_common::error::Error::Interrupted));
            }
            let line = line.map_err(|error| {
                if crate::stop::requested() {
                    agentlibre_common::error::Error::Interrupted.into()
                } else {
                    self.http.unreachable(error)
                }
            })?;
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let chunk: Json =
                serde_json::from_str(data).map_err(|error| self.http.bad_reply(error.to_string()))?;
            if let Some(message) = chunk["error"]["message"].as_str() {
                return Err(self.http.bad_reply(message));
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
        Err(self.http.bad_reply("the stream ended without a final chunk"))
    }

    fn count_tokens(&self, text: &str) -> Result<usize, Error> {
        let reply = self.http.json("/tokenize", Some(&json!({ "content": text })))?;
        reply["tokens"]
            .as_array()
            .map(Vec::len)
            .ok_or_else(|| self.http.bad_reply("no `tokens` in the /tokenize reply"))
    }

    fn context_size(&self) -> Result<usize, Error> {
        let reply = self.http.json("/props", None)?;
        reply["default_generation_settings"]["n_ctx"]
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| self.http.bad_reply("no `n_ctx` in the /props reply"))
    }

    fn models(&self) -> Result<Option<String>, Error> {
        let reply = self.http.json("/v1/models", None)?;
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
        let reply = self.http.json("/props", None)?;
        let vision = reply["modalities"]["vision"].as_bool().unwrap_or(false);
        match reply["media_marker"].as_str() {
            Some(marker) if vision => Ok(marker.to_owned()),
            _ => Err(BackendError::NoMultimodal.into()),
        }
    }

    fn hears_audio(&self) -> Result<bool, Error> {
        let reply = self.http.json("/props", None)?;
        Ok(reply["modalities"]["audio"].as_bool().unwrap_or(false))
    }
}

/// What the model is asked of a recording (AGL-051, D2): no language is named,
/// as the human may speak any.
const TRANSCRIBE_PROMPT: &str = "Transcribe the speech exactly, in the language it is spoken. Output only the transcript, with no comments.";

/// The most a transcript may take: a model that loops stops here. The
/// longest recording is minutes long, and speech is far under this.
const TRANSCRIBE_TOKENS: u32 = 4096;

/// The text before the first mark of the model: Gemma 4 12B can loop on its
/// empty block in place of a transcript, and that does not go to the input.
fn before_its_marks(text: &str) -> &str {
    text.find("<|").map_or(text, |at| &text[..at])
}

/// The body of a transcription: at temperature 0 and with no thinking.
fn transcribe_body(wav: &[u8]) -> Json {
    json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "input_audio", "input_audio": {"data": crate::media::base64(wav), "format": "wav"}},
                {"type": "text", "text": TRANSCRIBE_PROMPT},
            ],
        }],
        "temperature": 0,
        "max_tokens": TRANSCRIBE_TOKENS,
        "chat_template_kwargs": {"enable_thinking": false},
    })
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
pub fn messages_json(prompt: &Prompt) -> Vec<Json> {
    let kept = kept_thinking(prompt.messages, prompt.preserve_thinking);
    let mut out: Vec<Json> = prompt
        .messages
        .iter()
        .zip(kept)
        .map(|(message, keep_thinking)| match message {
            Message::System(text) => json!({"role": "system", "content": text}),
            Message::User(text) => json!({"role": "user", "content": text}),
            Message::Runtime(text) | Message::Wake(text) => {
                json!({"role": "user", "content": format!("{RUNTIME_PREFIX} {text}")})
            }
            Message::Assistant {
                thinking,
                text,
                call,
            } => {
                let text = text
                    .as_deref()
                    .filter(|_| call.is_none() || prompt.format.call.text_in_history);
                let mut json = json!({
                    "role": "assistant",
                    "content": text.unwrap_or_default(),
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
pub fn tool_json(doc: &Spec) -> Json {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in doc.params {
        let integer = param.kind() == Kind::Int;
        let mut property = json!({"type": if integer { "integer" } else { "string" }});
        if let Some(description) = param.description {
            property["description"] = description.into();
        }
        properties.insert(param.name.into(), property);
        if param.required {
            required.push(param.name);
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

#[cfg(test)]
mod server_test;
#[cfg(test)]
mod tests;
