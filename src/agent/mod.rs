//! The agent turn: steps of thinking and an action under the state
//! grammar until an answer or a stop (D11, D14, D19, D32, D33, D44, D45,
//! D94, D105, D108, D110, D113), and compaction of the history (D139,
//! D140).

use std::path::Path;

use crate::action::{self, Action, ParseError};
use crate::attach::Transcriber;
use crate::backend::{
    Backend, Message, Prompt, ReasoningLevel, Request, Stop, TOOL_DOCS, ToolCall,
    ToolDoc,
};
use crate::action::{Outcome, Value};
use crate::error::Error;
use crate::function::Function;
use crate::interrupt;
use crate::machine::{Machine, Progress};
use crate::media;
use crate::session::{Event, ForgeKind, INTERRUPTED, Session, StopReason};
use crate::tools::{self, ForgeEnv, ToolResult};

/// What the user sees while a turn runs.
pub trait Report {
    /// A visit of `state` begins.
    fn state(&mut self, state: &str);
    /// A piece of thinking as it streams.
    fn thinking(&mut self, piece: &str);
    /// Thinking of this step is over.
    fn thinking_done(&mut self);
    /// A piece of the action's text as it streams; never a piece of the
    /// call itself (D131).
    fn action_text(&mut self, _piece: &str) {}
    /// Text the model wrote before a call (D114), whole.
    fn text(&mut self, text: &str);
    /// The answer, whole; the turn ends (D131).
    fn answer(&mut self, _text: &str) {}
    fn call(&mut self, call: &ToolCall);
    fn result(&mut self, call: &ToolCall, result: &ToolResult);
    /// A write of the agent's forge tool (AGL-008): the marker line.
    fn forge(&mut self, _name: &str, _kind: ForgeKind, _section: Option<&str>) {}
    /// The summary is being generated (D140).
    fn compacting(&mut self) {}
    /// A summary replaced the history (D139).
    fn compacted(&mut self) {}
    /// An action hit `max_action_tokens` and was discarded (D154).
    fn cut_off(&mut self) {}
    /// The prompt of the next step has this many tokens (D152).
    fn context_used(&mut self, _tokens: usize) {}
    /// A generation decoded this many tokens (AGL-012).
    fn decoded(&mut self, _tokens: usize) {}
}

/// How a turn ended.
#[derive(Debug, PartialEq, Eq)]
pub enum TurnEnd {
    Answer(String),
    Stopped(StopReason),
}

/// What a compaction did.
#[derive(Debug, PartialEq, Eq)]
pub enum Compaction {
    Done,
    /// The history holds nothing besides the last summary.
    Nothing,
    /// The prompt and the summary do not fit the context, or the summary
    /// was cut off or empty (D140).
    Impossible,
}

/// Everything a turn needs.
pub struct Agent<'a> {
    pub function: &'a Function,
    /// The Function's name, for errors.
    pub function_name: &'a str,
    pub backend: &'a dyn Backend,
    pub session: &'a mut Session,
    pub workspace: &'a Path,
    /// The context size of the server, in tokens.
    pub context: usize,
    /// What the `forge` tool takes from the session (AGL-008).
    pub forge: ForgeEnv,
    /// The model's reasoning level (AGL-013); `None` is the template's
    /// default. `Off` silences the thinking states.
    pub reasoning: Option<ReasoningLevel>,
    /// The most tokens one picture takes: what the context check counts
    /// for every image and frame (AGL-014, D10).
    pub image_tokens: usize,
    /// Turns an audio the `view` tool opens into text when the model does
    /// not hear it (AGL-014, D22, D24).
    pub whisper: &'a dyn Transcriber,
}

impl Agent<'_> {
    /// Runs the turn for the user `message`. A stop is logged; a server
    /// error is logged as a stop and returned.
    pub fn turn(&mut self, message: &str, report: &mut dyn Report) -> Result<TurnEnd, Error> {
        self.session.append(Event::User(message.into()))?;
        let unfinished = self.session.unfinished().expect("the turn has a message");
        let mut machine = Machine::resume(self.function, self.function_name, &unfinished)?;
        if unfinished.state.is_none() {
            self.enter(machine.state().name.as_str(), report)?;
        }
        loop {
            let result = self.step(&mut machine, report);
            let stop = match result {
                Ok(Some(end)) => return Ok(end),
                Ok(None) => continue,
                Err(Error::Interrupted) => StopReason::Interrupted,
                Err(
                    error @ (Error::ServerUnreachable { .. }
                    | Error::ServerStatus { .. }
                    | Error::ServerReply { .. }
                    | Error::OutsideGrammar(_)),
                ) => {
                    self.session.append(Event::Stop(StopReason::ServerError))?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            self.session.append(Event::Stop(stop))?;
            return Ok(TurnEnd::Stopped(stop));
        }
    }

    fn enter(&mut self, state: &str, report: &mut dyn Report) -> Result<(), Error> {
        self.session.append(Event::State(state.into()))?;
        report.state(state);
        Ok(())
    }

    /// One step; `None` when the turn goes on.
    fn step(
        &mut self,
        machine: &mut Machine,
        report: &mut dyn Report,
    ) -> Result<Option<TurnEnd>, Error> {
        let function = self.function;
        let format = &function.format;
        let state = machine.state();
        let mut messages = vec![Message::System(system_prompt(function))];
        messages.extend(self.session.history());
        // The media the messages name become the server's marker, the
        // frames and the transcripts (AGL-014, D6).
        let expanded = media::expand(
            &mut messages,
            &self.session.media(),
            &self.session.media_dir(),
            self.backend,
        )?;
        let tools = function_tools(function);
        // `Off` silences the thinking states (AGL-013, Q4): the level and
        // the state decide the thinking together.
        let think = state.think && self.reasoning != Some(ReasoningLevel::Off);
        let rendered = self.backend.render(&Prompt {
            messages: &messages,
            instruction: state.instruction.as_deref(),
            tools: &tools,
            preserve_thinking: function.preserve_thinking,
            reasoning: self.reasoning,
        })?;
        let mut prompt = format.start_step(rendered, think);

        // The step must fit the context with its full generation (D32).
        let think_budget = if think {
            function.max_think_tokens as usize
        } else {
            0
        };
        // A picture is counted by its bound, not by the marker (D10).
        let prompt_tokens =
            self.backend.count_tokens(&prompt)? + expanded.pictures * self.image_tokens;
        report.context_used(prompt_tokens);
        let needed = prompt_tokens + think_budget + function.max_action_tokens as usize;
        if needed > self.context {
            // A summary of the history may make room (D139, D140).
            return match self.compact(report)? {
                Compaction::Done => Ok(None),
                Compaction::Nothing | Compaction::Impossible => self.stop(StopReason::ContextFull),
            };
        }

        if let Some(markers) = format.think.as_ref().filter(|_| think) {
            let close = markers.close.trim();
            let call_start = format.call.begin_before.trim();
            let thinking = self.backend.generate(
                &Request {
                    prompt: &prompt,
                    grammar: None,
                    stop: &[close, call_start],
                    max_tokens: function.max_think_tokens,
                    sampling: &function.sampling,
                    media: &expanded.uploads,
                },
                &mut |piece| report.thinking(piece),
            )?;
            report.thinking_done();
            if !thinking.text.is_empty() {
                report.decoded(self.backend.count_tokens(&thinking.text)?);
            }
            let text = thinking.text.trim_end();
            if !text.is_empty() {
                self.session.append(Event::Thinking(text.into()))?;
            }
            // A cut-off thinking or a call started inside it ends the
            // thinking too (D110, D113).
            prompt.push_str(text);
            prompt.push_str(&markers.close);
        }

        let (grammar, root) = action::grammar(format, &state.actions);
        let mut stream = TextStream::new(&format.call.begin_before);
        let generated = self.backend.generate(
            &Request {
                prompt: &prompt,
                grammar: Some(&grammar.render(&root)),
                stop: &[],
                max_tokens: function.max_action_tokens,
                sampling: &function.sampling,
                media: &expanded.uploads,
            },
            &mut |piece| {
                if let Some(text) = stream.push(piece) {
                    report.action_text(&text);
                }
            },
        )?;
        if !generated.text.is_empty() {
            report.decoded(self.backend.count_tokens(&generated.text)?);
        }
        if let Some(text) = stream.finish() {
            report.action_text(&text);
        }
        // Cut-off text may have been heading for a call: it is discarded
        // and the model is told to split the work (D154).
        if generated.stop == Stop::Limit {
            self.session
                .append(Event::CutOff(function.max_action_tokens))?;
            report.cut_off();
            let progress = machine.after_cut_off();
            if machine.entered() {
                self.enter(machine.state().name.as_str(), report)?;
            }
            return match progress {
                Progress::Continue => Ok(None),
                Progress::StepLimit => self.stop(StopReason::StepLimit),
            };
        }
        let action = match action::parse(format, &state.actions, &generated.text) {
            Ok(action) => action,
            // The grammar allows nothing else, so the server broke it.
            Err(ParseError::Incomplete | ParseError::Unexpected { .. }) => {
                return Err(Error::OutsideGrammar(generated.text));
            }
        };
        match action {
            Action::Answer(text) => {
                self.session.append(Event::Answer(text.clone()))?;
                report.answer(&text);
                Ok(Some(TurnEnd::Answer(text)))
            }
            Action::Call { text, tool, args } => {
                let call = ToolCall {
                    id: self.session.next_call_id(),
                    name: tool,
                    args,
                };
                self.session.append(Event::Call {
                    text: text.clone(),
                    call: call.clone(),
                })?;
                if let Some(text) = &text {
                    report.text(text);
                }
                report.call(&call);
                let (result, write) = if call.name == "view" {
                    (self.view(&call.args), None)
                } else {
                    tools::run(self.workspace, self.forge, &call.name, &call.args)
                };
                self.session.append(Event::Result {
                    id: call.id.clone(),
                    outcome: result.outcome,
                    content: result.content.clone(),
                })?;
                report.result(&call, &result);
                if let Some(write) = write {
                    self.session.append(Event::Forge {
                        name: write.name.clone(),
                        kind: write.kind,
                        section: write.section.clone(),
                    })?;
                    report.forge(&write.name, write.kind, write.section.as_deref());
                }
                if interrupt::interrupted() {
                    debug_assert_eq!(result.content, INTERRUPTED);
                    return Err(Error::Interrupted);
                }
                let progress = machine.after_call(&call.name, result.outcome);
                if machine.entered() {
                    self.enter(machine.state().name.as_str(), report)?;
                }
                match progress {
                    Progress::Continue => Ok(None),
                    Progress::StepLimit => self.stop(StopReason::StepLimit),
                }
            }
        }
    }

    /// The `view` tool (AGL-014, D24): keeps the file as a medium of the
    /// session, as `/attach` does, and answers with its placeholder, which
    /// becomes the marker, the frames or the transcript at the next step.
    fn view(&mut self, args: &[(String, Value)]) -> ToolResult {
        let fail = |content: String| ToolResult {
            outcome: Outcome::Fail,
            content,
        };
        let Some(path) = args.iter().find_map(|(name, value)| match value {
            Value::Text(text) if name == "path" => Some(text.as_str()),
            _ => None,
        }) else {
            return fail("view needs a path.".into());
        };
        let path = self.workspace.join(path);
        let source = match media::Source::from_file(&path) {
            Ok(source) => source,
            Err(Error::MediaKind { .. }) => {
                return fail(format!(
                    "{} is not an image, a video or an audio.",
                    path.display()
                ));
            }
            Err(error) => return fail(error.message(crate::i18n::Lang::En)),
        };
        // The picture needs the projector; ask before the file is kept.
        if source.kind != media::Kind::Audio && self.backend.media_marker().is_err() {
            return fail("The server has no multimodal projector, so it cannot take images.".into());
        }
        let transcript = if source.kind == media::Kind::Audio {
            match self.backend.hears_audio() {
                Ok(true) => None,
                Ok(false) => match self.whisper.transcribe(&source, &mut |_| {}) {
                    Ok(text) => Some(text),
                    Err(error) => return fail(error.message(crate::i18n::Lang::En)),
                },
                Err(error) => return fail(error.message(crate::i18n::Lang::En)),
            }
        } else {
            None
        };
        let number = self.session.next_media_number(source.kind);
        match self.session.store_media(&source, number, transcript) {
            Ok(kept) => ToolResult {
                outcome: Outcome::Ok,
                content: format!("{} {}", kept.placeholder(), kept.name),
            },
            Err(error) => fail(error.message(crate::i18n::Lang::En)),
        }
    }

    /// Replaces the history with a summary the model writes without
    /// thinking or a grammar, from the history without thinking (D139,
    /// D140).
    pub fn compact(&mut self, report: &mut dyn Report) -> Result<Compaction, Error> {
        if !self.session.compactable() {
            return Ok(Compaction::Nothing);
        }
        report.compacting();
        let function = self.function;
        let mut messages = vec![Message::System(system_prompt(function))];
        messages.extend(
            self.session
                .history()
                .into_iter()
                .map(|message| match message {
                    Message::Assistant { text, call, .. } => Message::Assistant {
                        thinking: None,
                        text,
                        call,
                    },
                    other => other,
                }),
        );
        let expanded = media::expand(
            &mut messages,
            &self.session.media(),
            &self.session.media_dir(),
            self.backend,
        )?;
        let tools = function_tools(function);
        let rendered = self.backend.render(&Prompt {
            messages: &messages,
            instruction: Some(&function.compact),
            tools: &tools,
            preserve_thinking: false,
            reasoning: self.reasoning,
        })?;
        let prompt = function.format.start_step(rendered, false);
        let needed = self.backend.count_tokens(&prompt)?
            + expanded.pictures * self.image_tokens
            + function.max_compact_tokens as usize;
        if needed > self.context {
            return Ok(Compaction::Impossible);
        }
        let generated = self.backend.generate(
            &Request {
                prompt: &prompt,
                grammar: None,
                stop: &[],
                max_tokens: function.max_compact_tokens,
                sampling: &function.sampling,
                media: &expanded.uploads,
            },
            &mut |_| {},
        )?;
        let summary = generated.text.trim();
        if generated.stop == Stop::Limit || summary.is_empty() {
            return Ok(Compaction::Impossible);
        }
        self.session.append(Event::Compact(summary.into()))?;
        report.compacted();
        Ok(Compaction::Done)
    }

    /// Logs a stop and ends the turn with it.
    fn stop(&mut self, reason: StopReason) -> Result<Option<TurnEnd>, Error> {
        self.session.append(Event::Stop(reason))?;
        Ok(Some(TurnEnd::Stopped(reason)))
    }
}

/// Cuts the call out of the streamed action: passes the text before the
/// start of a call and holds back what may still turn into that start
/// (D131).
struct TextStream<'a> {
    begin: &'a str,
    held: String,
    in_call: bool,
}

impl<'a> TextStream<'a> {
    fn new(begin: &'a str) -> TextStream<'a> {
        TextStream {
            begin,
            held: String::new(),
            in_call: false,
        }
    }

    /// The text to show for `piece`, if any.
    fn push(&mut self, piece: &str) -> Option<String> {
        if self.in_call {
            return None;
        }
        self.held.push_str(piece);
        let (shown, kept) = match self.held.find(self.begin) {
            Some(start) => {
                self.in_call = true;
                (start, self.held.len())
            }
            None => {
                let keep_from = self
                    .held
                    .char_indices()
                    .map(|(index, _)| index)
                    .find(|index| self.begin.starts_with(&self.held[*index..]))
                    .unwrap_or(self.held.len());
                (keep_from, keep_from)
            }
        };
        let text = self.held[..shown].to_owned();
        if self.in_call {
            self.held.clear();
        } else {
            self.held.drain(..kept);
        }
        (!text.is_empty()).then_some(text)
    }

    /// The held-back text when no call began.
    fn finish(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.held);
        (!self.in_call && !text.is_empty()).then_some(text)
    }
}

/// The system message of a step and of a compaction: the Function's
/// `system` and nothing the runtime adds (AGL-017, D3, D4). What the
/// runtime prefix means and how large an action may be (AGL-009) is in the
/// `system` of the built-in `agentLIBRE` Function, not here. Stable text, for
/// the prompt cache (D14, D19).
fn system_prompt(function: &Function) -> String {
    function.system.clone()
}

/// Every tool of any state, for the stable tool list of the prompt (D14,
/// D113).
fn function_tools(function: &Function) -> Vec<&'static ToolDoc> {
    TOOL_DOCS
        .iter()
        .filter(|doc| {
            function
                .states
                .iter()
                .any(|state| state.actions.tools.iter().any(|t| t.name == doc.name))
        })
        .collect()
}

#[cfg(test)]
mod tests;
