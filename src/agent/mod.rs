//! The agent turn: steps of thinking and an action under the state
//! grammar until an answer or a stop (D11, D14, D19, D32, D33, D44, D45,
//! D94, D105, D108, D110, D113), and compaction of the history (D139,
//! D140).

use std::path::Path;

use crate::action::{self, Action};
use crate::attach::Transcriber;
use crate::backend::{
    Backend, Message, Prefill, Prompt, PromptProgress, ReasoningLevel, Request, Stop, TOOL_DOCS, ToolCall,
    ToolDoc,
};
use crate::action::{Outcome, Value};
use crate::error::Error;
use crate::function::Function;
use crate::interrupt;
use crate::machine::{Machine, Progress};
use crate::media;
use crate::phase::{Observed, Observer, PhaseEvent, StepRecord, Transition};
use crate::search::Searcher;
use crate::session::{CutOff, Event, ForgeKind, INTERRUPTED, Session, StopReason};
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
    /// A write of the memory tool (AGL-006): the marker line, then the text
    /// that was written.
    fn memory(&mut self, _name: &str, _kind: crate::memory::Kind, _text: Option<&str>) {}
    /// The summary is being generated (D140).
    fn compacting(&mut self) {}
    /// A summary replaced the history (D139).
    fn compacted(&mut self) {}
    /// A generation hit its budget and was discarded (D154; AGL-043 F8).
    fn cut_off(&mut self, _cut: &CutOff) {}
    /// A step begins: its prompt is rendered, counted and read by the
    /// server until the first token (AGL-043 F2).
    fn prefill(&mut self) {}
    /// The prompt of the next step has this many tokens (D152).
    fn context_used(&mut self, _tokens: usize) {}
    /// A generation decoded this many tokens (AGL-012).
    fn decoded(&mut self, _tokens: usize) {}
    /// A stir of the stirred mode: the `attempt`-th corrective retry of the
    /// step, of at most `max`, after the malformed output named by `error`
    /// (AGL-030, D4; AGL-043 F8).
    fn stirred(&mut self, _attempt: u32, _max: u32, _error: &str) {}
    /// A compaction that started made no summary (AGL-046).
    fn compact_failed(&mut self) {}
    /// A transition of the FSM during the turn (AGL-046).
    fn transition(&mut self, _transition: &Transition) {}
    /// How the reading of a prompt goes, as the server says (AGL-046, D19).
    fn prompt_progress(&mut self, _progress: PromptProgress) {}
    /// How the server read the prompt of a generation (AGL-046).
    fn prompt_read(&mut self, _prefill: Prefill) {}
    /// What a step's prompt is made of (AGL-046, D15).
    fn step_started(&mut self, _record: &StepRecord) {}
}

/// What starts the runtime message with the index of the memory.
const MEMORY_MESSAGE: &str = "workspace memory\n";

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
    /// The prompt and the summary of at most `limit` tokens do not fit
    /// the context, or the summary was empty (D140; AGL-043 F6).
    Impossible { limit: u32 },
}

/// Everything a turn needs.
pub struct Agent<'a> {
    pub function: &'a Function,
    /// The Function's name, for errors.
    pub function_name: &'a str,
    pub backend: &'a dyn Backend,
    /// The stirred mode (AGL-030, D1, D4): the action phase generates
    /// unconstrained and a parse failure is a bounded corrective retry
    /// instead of a stop. Set from the settings' `[server] kind`.
    pub stirred: bool,
    pub session: &'a mut Session,
    pub workspace: &'a Path,
    /// The context size of the server, in tokens.
    pub context: usize,
    /// What the `forge` tool takes from the session (AGL-008).
    pub forge: ForgeEnv<'a>,
    /// The model's reasoning level (AGL-013); `None` is the template's
    /// default. `Off` silences the thinking states.
    pub reasoning: Option<ReasoningLevel>,
    /// The most tokens one picture takes: what the context check counts
    /// for every image and frame (AGL-014, D10).
    pub image_tokens: usize,
    /// Turns an audio the `view` tool opens into text when the model does
    /// not hear it (AGL-014, D22, D24).
    pub whisper: &'a dyn Transcriber,
    /// Runs the `search` tool (AGL-019); a profile without `[search]`
    /// has no such tool, so its searcher is never asked.
    pub search: &'a dyn Searcher,
    /// Where the memory of the workspace is (AGL-006).
    pub memory: &'a dyn crate::memory::Home,
}

impl Agent<'_> {
    /// Runs the turn for the user `message`, with an observer of its own.
    #[cfg(test)]
    pub fn turn(&mut self, message: &str, report: &mut dyn Report) -> Result<TurnEnd, Error> {
        let mut observer = Observer::new();
        observer.attach(self.session);
        self.turn_observed(message, report, &mut observer)
    }

    /// Runs the turn for the user `message`. A stop is logged; a server
    /// error is logged as a stop and returned. `observer` saw what came
    /// before the turn: the start of the server and the transcription
    /// (AGL-046, D10).
    pub fn turn_observed(
        &mut self,
        message: &str,
        report: &mut dyn Report,
        observer: &mut Observer,
    ) -> Result<TurnEnd, Error> {
        let mut observed = Observed { observer, report };
        let end = self.run_turn(message, &mut observed);
        match &end {
            Ok(TurnEnd::Answer(_)) => {}
            Ok(TurnEnd::Stopped(reason)) => observed.end(PhaseEvent::Stop(*reason)),
            Err(_) => observed.end(PhaseEvent::Failed),
        }
        end
    }

    fn run_turn(&mut self, message: &str, report: &mut dyn Report) -> Result<TurnEnd, Error> {
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
        report.prefill();
        let function = self.function;
        let format = &function.format;
        let state = machine.state();
        let mut messages = self.step_messages();
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
        // A step that does not think is rendered at `Off`, so the template
        // and the closed thinking block agree, as in the stirred mode
        // (AGL-043 F16).
        let reasoning = if think {
            self.reasoning
        } else {
            Some(ReasoningLevel::Off)
        };
        let step_prompt = Prompt {
            messages: &messages,
            instruction: state.instruction.as_deref(),
            tools: &tools,
            preserve_thinking: function.preserve_thinking,
            reasoning,
            format,
        };
        let rendered = self.backend.render(&step_prompt)?;
        let mut prompt = format.start_step(rendered, think);

        // The step must fit the context with its full generation (D32).
        let think_budget = if think {
            function.max_think_tokens as usize
        } else {
            0
        };
        // A picture is counted by its bound, not by the marker (D10).
        let prompt_tokens = self.backend.count_prompt(&step_prompt, &prompt)?
            + expanded.pictures * self.image_tokens;
        report.context_used(prompt_tokens);
        self.record_step(report, state, &messages, &tools, &prompt, prompt_tokens, think)?;
        // The early compaction (AGL-038, D6, D9): the history plus the most
        // a step can add and the summary must fit, so the summary fits at
        // the next step too; one that makes no room lets the step run when
        // it fits.
        if function.compact_early
            && prompt_tokens + early_reserve(function, self.context) > self.context
            && self.compact_history(report)? == Compaction::Done
        {
            return Ok(None);
        }
        let action_budget = function.action_tokens(self.context);
        let needed = prompt_tokens + think_budget + action_budget as usize;
        if needed > self.context {
            // A summary of the history may make room (D139, D140).
            return match self.compact_history(report)? {
                Compaction::Done => Ok(None),
                Compaction::Nothing | Compaction::Impossible { .. } => {
                    self.stop(StopReason::ContextFull)
                }
            };
        }

        // The stirred mode generates the whole step in one structured
        // request (AGL-030, D9).
        if self.stirred {
            return self.stirred_step(machine, report, messages, &expanded.uploads, &tools, think);
        }

        if let Some(markers) = format.think.as_ref().filter(|_| think) {
            let close = markers.close.trim();
            let call_start = format.call.begin_before.trim();
            let cell = std::cell::RefCell::new(&mut *report);
            let thinking = self.backend.generate_reading(
                &Request {
                    prompt: &prompt,
                    grammar: None,
                    stop: &[close, call_start],
                    max_tokens: function.max_think_tokens,
                    sampling: &function.sampling,
                    media: &expanded.uploads,
                    structured: None,
                },
                Some(&mut |progress| cell.borrow_mut().prompt_progress(progress)),
                &mut |_| {},
                &mut |piece| cell.borrow_mut().thinking(piece),
            )?;
            report.thinking_done();
            if let Some(prefill) = thinking.prefill {
                report.prompt_read(prefill);
            }
            if !thinking.text.is_empty() {
                report.decoded(self.backend.count_tokens(&thinking.text)?);
            }
            let text = thinking.text.trim_end().to_owned();
            if !text.is_empty() {
                self.session.append(Event::Thinking(text.clone()))?;
            }
            // A cut-off thinking or a call started inside it ends the
            // thinking too (D110, D113).
            prompt.push_str(&text);
            prompt.push_str(&markers.close);
        }

        // The grammar of the actions (D3): the action phase generates under
        // it.
        let (grammar, root) = action::grammar(format, &state.actions);
        let grammar_str = grammar.render(&root);
        self.session.record_input("grammar", &grammar_str)?;
        let mut stream = TextStream::new(&format.call.begin_before);
        let cell = std::cell::RefCell::new(&mut *report);
        let generated = self.backend.generate_reading(
            &Request {
                prompt: &prompt,
                grammar: Some(&grammar_str),
                stop: &[],
                max_tokens: action_budget,
                sampling: &function.sampling,
                media: &expanded.uploads,
                structured: None,
            },
            Some(&mut |progress| cell.borrow_mut().prompt_progress(progress)),
            &mut |_| {},
            &mut |piece| {
                if let Some(text) = stream.push(piece) {
                    cell.borrow_mut().action_text(&text);
                }
            },
        )?;
        if let Some(prefill) = generated.prefill {
            report.prompt_read(prefill);
        }
        let action_tokens = if generated.text.is_empty() {
            0
        } else {
            self.backend.count_tokens(&generated.text)?
        };
        if action_tokens > 0 {
            report.decoded(action_tokens);
        }
        if let Some(text) = stream.finish() {
            report.action_text(&text);
        }
        // Cut-off text may have been heading for a call: it is discarded
        // and the model is told to split the work (D154).
        if generated.stop == Stop::Limit {
            let cut = CutOff {
                limit: action_budget,
                thinking: None,
                discarded: u32::try_from(action_tokens).unwrap_or(u32::MAX),
                call: generated.text.contains(format.call.begin_before.trim()),
            };
            return self.cut_off(machine, report, cut);
        }
        // The grammar allows nothing else, so the server broke it.
        let action = action::parse(format, &state.actions, &generated.text)
            .map_err(|_| Error::OutsideGrammar(generated.text.clone()))?;
        self.run_action(machine, report, action)
    }

    /// The step of the stirred mode (AGL-030, D4, D9, D10): one structured
    /// generation — the server splits the thinking and the call from the
    /// text —, the response mapped to an action by its structure, and a
    /// malformed one a bounded corrective retry (the stir), then a stop.
    fn stirred_step(
        &mut self,
        machine: &mut Machine,
        report: &mut dyn Report,
        mut messages: Vec<Message>,
        uploads: &[String],
        tools: &[&'static ToolDoc],
        think: bool,
    ) -> Result<Option<TurnEnd>, Error> {
        let function = self.function;
        let state = machine.state();
        let state_name = state.name.clone();
        let instruction = state.instruction.clone();
        let actions = state.actions.clone();
        // A state that does not think, or the `Off` level, silences the
        // thinking (AGL-013, Q4).
        let reasoning = if think {
            self.reasoning
        } else {
            Some(ReasoningLevel::Off)
        };
        let max_tokens = function.action_tokens(self.context)
            + if think { function.max_think_tokens } else { 0 };
        let mut stirs = 0u32;
        let (generated, action) = loop {
            let prompt = Prompt {
                messages: &messages,
                instruction: instruction.as_deref(),
                tools,
                preserve_thinking: function.preserve_thinking,
                reasoning,
                format: &function.format,
            };
            // The thinking and the text stream to the same report; the
            // thinking is over at the first piece of text.
            let open = std::cell::Cell::new(think);
            let cell = std::cell::RefCell::new(&mut *report);
            let generated = self.backend.generate_reading(
                &Request {
                    prompt: "",
                    grammar: None,
                    stop: &[],
                    max_tokens,
                    sampling: &function.sampling,
                    media: uploads,
                    structured: Some(&prompt),
                },
                Some(&mut |progress| cell.borrow_mut().prompt_progress(progress)),
                &mut |piece| cell.borrow_mut().thinking(piece),
                &mut |piece| {
                    if open.replace(false) {
                        cell.borrow_mut().thinking_done();
                    }
                    cell.borrow_mut().action_text(piece);
                },
            )?;
            if open.replace(false) {
                cell.borrow_mut().thinking_done();
            }
            if let Some(prefill) = generated.prefill {
                report.prompt_read(prefill);
            }
            // The exact counts of the server's `usage` (AGL-030, D11).
            if let Some(tokens) = generated.prompt_tokens {
                report.context_used(tokens);
            }
            let thinking_tokens = || -> Result<usize, Error> {
                match &generated.thinking {
                    Some(thinking) => self.backend.count_tokens(thinking),
                    None => Ok(0),
                }
            };
            let decoded = match generated.completion_tokens {
                Some(tokens) => tokens,
                None => self.backend.count_tokens(&generated.text)? + thinking_tokens()?,
            };
            if decoded > 0 {
                report.decoded(decoded);
            }
            // A step that hit its budget is a miss of the budget, not of
            // the model: discarded, and the model is told to split the
            // work (D154).
            if generated.stop == Stop::Limit {
                let tokens = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                let thinking = if think {
                    Some(tokens(thinking_tokens()?))
                } else {
                    None
                };
                let cut = CutOff {
                    limit: max_tokens,
                    // The thinking took its part of the one budget
                    // (AGL-043 F8).
                    thinking,
                    discarded: tokens(self.backend.count_tokens(&generated.text)?),
                    call: !generated.tool_calls.is_empty()
                        || generated.text.contains(function.format.call.begin_before.trim()),
                };
                return self.cut_off(machine, report, cut);
            }
            match action::from_structured(&actions, &generated.text, &generated.tool_calls) {
                Ok(action) => break (generated, action),
                Err(detail) => {
                    if stirs >= function.max_stirs {
                        return self.stop(StopReason::StirLimit);
                    }
                    stirs += 1;
                    self.session.append(Event::Stir {
                        attempt: stirs,
                        error: detail.clone(),
                    })?;
                    report.stirred(stirs, function.max_stirs, &detail);
                    // The malformed output is discarded; the retry sees
                    // the nudge as a runtime message (AGL-030, D4, D8).
                    messages.push(Message::Runtime(stir_nudge(&state_name, &detail)));
                }
            }
        };
        if let Some(thinking) = generated
            .thinking
            .as_deref()
            .map(str::trim_end)
            .filter(|thinking| !thinking.is_empty())
        {
            self.session.append(Event::Thinking(thinking.into()))?;
        }
        self.run_action(machine, report, action)
    }

    /// The action of the step ran out of tokens (D154): it is recorded and
    /// discarded, and the machine moves on.
    fn cut_off(
        &mut self,
        machine: &mut Machine,
        report: &mut dyn Report,
        cut: CutOff,
    ) -> Result<Option<TurnEnd>, Error> {
        report.cut_off(&cut);
        self.session.append(Event::CutOff(cut))?;
        let progress = machine.after_cut_off();
        if machine.entered() {
            self.enter(machine.state().name.as_str(), report)?;
        }
        match progress {
            Progress::Continue => Ok(None),
            Progress::StepLimit => self.stop(StopReason::StepLimit),
        }
    }

    /// Runs the action of the step: the answer ends the turn, a call runs
    /// its tool and the machine takes its outcome.
    fn run_action(
        &mut self,
        machine: &mut Machine,
        report: &mut dyn Report,
        action: Action,
    ) -> Result<Option<TurnEnd>, Error> {
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
                let (result, write, memory_write) = match call.name.as_str() {
                    "view" => (self.view(&call.args), None, None),
                    "search" => (self.search(&call.args), None, None),
                    "memory" => {
                        let (result, write) = self.memory_tool(&call.args, text.as_deref());
                        (result, None, write)
                    }
                    _ => {
                        let (result, write) =
                            tools::run(self.workspace, self.forge, &call.name, &call.args);
                        (result, write, None)
                    }
                };
                self.session.append(Event::Result {
                    id: call.id.clone(),
                    outcome: result.outcome,
                    content: result.content.clone(),
                })?;
                report.result(&call, &result);
                if let Some(write) = memory_write {
                    self.session.append(Event::Memory {
                        name: write.name.clone(),
                        kind: write.kind,
                        text: write.text.clone(),
                    })?;
                    report.memory(&write.name, write.kind, write.text.as_deref());
                }
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

    /// The `search` tool (AGL-019): a call the service or the check of its
    /// arguments refuses is a failed result the model reads, not a stop
    /// (D7).
    fn search(&self, args: &[(String, Value)]) -> ToolResult {
        match crate::search::Request::from_args(args) {
            Ok(request) => self.search.search(&request),
            Err(sentence) => ToolResult {
                outcome: Outcome::Fail,
                content: sentence,
            },
        }
    }

    /// The `memory` tool (AGL-006, M32): `add` registers the workspace's
    /// store when it is not yet (M23); the other actions need the memory
    /// that is there. `call_text` is the text of the call that preceded it
    /// (AGL-029, D1): the body of `add` when its `text` field is empty.
    fn memory_tool(
        &mut self,
        args: &[(String, Value)],
        call_text: Option<&str>,
    ) -> (ToolResult, Option<crate::memory::Write>) {
        let args = crate::memory::Args::from_call(args);
        let fail = |content: String| {
            (
                ToolResult {
                    outcome: Outcome::Fail,
                    content,
                },
                None,
            )
        };
        let (dir, lines) = if args.action == "add" {
            match self.memory.write() {
                Ok(found) => found,
                Err(error) => return fail(error.message(crate::i18n::Lang::En)),
            }
        } else {
            match self.memory.read() {
                Some(dir) => (dir, Vec::new()),
                None => return fail("The memory of this workspace has no entries yet.".into()),
            }
        };
        let (mut result, write) = crate::memory::run(&dir, &args, call_text);
        // A store that moved or was adopted on the way: said once.
        if !lines.is_empty() {
            result.content.insert_str(0, &(lines.join("\n") + "\n"));
        }
        (result, write)
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

    /// Records what the prompt of a step is made of (AGL-046, D14, D15):
    /// each part in full when it changed, and the step's hash and budgets.
    #[allow(clippy::too_many_arguments)]
    fn record_step(
        &mut self,
        report: &mut dyn Report,
        state: &crate::function::State,
        messages: &[Message],
        tools: &[&'static ToolDoc],
        prompt: &str,
        prompt_tokens: usize,
        think: bool,
    ) -> Result<(), Error> {
        let function = self.function;
        if let Some(Message::System(system)) = messages.first() {
            self.session.record_input("system", system)?;
        }
        let tool_docs: Vec<String> = tools
            .iter()
            .map(|doc| format!("{}: {} {:?}", doc.name, doc.description, doc.params))
            .collect();
        self.session.record_input("tools", &tool_docs.join("\n"))?;
        let memory = match messages.get(1) {
            Some(Message::Runtime(text)) => text.strip_prefix(MEMORY_MESSAGE).unwrap_or(""),
            _ => "",
        };
        self.session.record_input("memory_index", memory)?;
        self.session
            .record_input("sampling", &format!("{:?}", function.sampling))?;
        self.session
            .record_input("model", &format!("{:?}", function.model))?;
        report.step_started(&StepRecord {
            state: &state.name,
            prompt_sha256: media::sha256_hex(prompt.as_bytes()),
            prompt_tokens,
            think,
            think_budget: if think {
                function.max_think_tokens as usize
            } else {
                0
            },
            action_budget: function.action_tokens(self.context),
            context: self.context,
            stirred: self.stirred,
            reasoning: format!("{:?}", self.reasoning),
        });
        Ok(())
    }

    /// The messages of a step before its state's instruction: the system
    /// prompt, the index of the workspace's memory, which is not stored
    /// (AGL-006, M27), then the history. The server's cache holds only if the
    /// next prompt continues this one, so nothing stands after the history
    /// that the next step's history would go before (AGL-047, D1).
    fn step_messages(&mut self) -> Vec<Message> {
        let mut messages = vec![Message::System(system_prompt(self.function, self.context))];
        let memory = self.memory;
        let index = self
            .session
            .memory_index(|| memory.read().and_then(|dir| crate::memory::index_text(&dir)));
        if let Some(index) = index {
            messages.push(Message::Runtime(format!("{MEMORY_MESSAGE}{index}")));
        }
        messages.extend(self.session.history());
        messages
    }

    /// The tokens of the history as the next step sends it, without its
    /// state's instruction (AGL-043 F7): what the dock shows after a
    /// compaction, also when no step follows it (the REPL's `/compact`).
    fn history_tokens(&mut self) -> Result<usize, Error> {
        let function = self.function;
        let mut messages = self.step_messages();
        let expanded = media::expand(
            &mut messages,
            &self.session.media(),
            &self.session.media_dir(),
            self.backend,
        )?;
        let tools = function_tools(function);
        let prompt = Prompt {
            messages: &messages,
            instruction: None,
            tools: &tools,
            preserve_thinking: function.preserve_thinking,
            reasoning: self.reasoning,
            format: &function.format,
        };
        let flat = function.format.start_step(self.backend.render(&prompt)?, false);
        Ok(self.backend.count_prompt(&prompt, &flat)? + expanded.pictures * self.image_tokens)
    }

    /// One summary request of `instruction` over `base`, within `budget`
    /// tokens: the history is cut until the request fits (AGL-038, D5);
    /// `None` when it cannot fit.
    fn summarize(
        &self,
        base: &mut Vec<Message>,
        instruction: &str,
        budget: u32,
    ) -> Result<Option<crate::backend::Generated>, Error> {
        let function = self.function;
        let tools = function_tools(function);
        let (messages, expanded, prompt) = loop {
            let mut messages = base.clone();
            let expanded = media::expand(
                &mut messages,
                &self.session.media(),
                &self.session.media_dir(),
                self.backend,
            )?;
            let compact_prompt = Prompt {
                messages: &messages,
                instruction: Some(instruction),
                tools: &tools,
                preserve_thinking: false,
                reasoning: self.reasoning,
                format: &function.format,
            };
            let rendered = self.backend.render(&compact_prompt)?;
            let prompt = function.format.start_step(rendered, false);
            let needed = self.backend.count_prompt(&compact_prompt, &prompt)?
                + expanded.pictures * self.image_tokens
                + budget as usize;
            if needed <= self.context {
                break (messages, expanded, prompt);
            }
            if !cut_for_summary(base, needed - self.context) {
                return Ok(None);
            }
        };
        // In the stirred mode the summary is a structured generation too,
        // with the thinking off and no tool to call (AGL-030, D9).
        let structured = Prompt {
            messages: &messages,
            instruction: Some(instruction),
            tools: &[],
            preserve_thinking: false,
            reasoning: Some(ReasoningLevel::Off),
            format: &function.format,
        };
        let generated = self.backend.generate(
            &Request {
                prompt: &prompt,
                grammar: None,
                stop: &[],
                max_tokens: budget,
                sampling: &function.sampling,
                media: &expanded.uploads,
                structured: self.stirred.then_some(&structured),
            },
            &mut |_| {},
            &mut |_| {},
        )?;
        Ok(Some(generated))
    }

    /// Replaces the history with a summary the model writes without
    /// thinking or a grammar, from the history without thinking (D139,
    /// D140). A history whose summary request does not fit is cut until it
    /// does (AGL-038, D5): its tool results, the oldest first, to their
    /// head and tail, then its oldest turns; it is impossible only when the
    /// latest turn alone does not fit. The question that started the turn
    /// is kept after the summary, so the turn goes on from the summary and
    /// the question together and the summary need not repeat it (F31).
    pub fn compact(&mut self, report: &mut dyn Report) -> Result<Compaction, Error> {
        let mut observer = Observer::new();
        observer.attach_between_turns(self.session);
        let mut observed = Observed {
            observer: &mut observer,
            report,
        };
        let result = self.compact_history(&mut observed);
        observed.end(if result.is_ok() {
            PhaseEvent::Idle
        } else {
            PhaseEvent::Failed
        });
        result
    }

    /// The compaction inside a turn or of [`Agent::compact`].
    fn compact_history(&mut self, report: &mut dyn Report) -> Result<Compaction, Error> {
        let result = self.summarize_history(report)?;
        if matches!(result, Compaction::Impossible { .. }) {
            report.compact_failed();
        }
        Ok(result)
    }

    fn summarize_history(&mut self, report: &mut dyn Report) -> Result<Compaction, Error> {
        if !self.session.compactable() {
            return Ok(Compaction::Nothing);
        }
        report.compacting();
        let function = self.function;
        let budget = function.compact_tokens(self.context);
        let mut base = vec![Message::System(system_prompt(function, self.context))];
        base.extend(
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
        // A summary that reaches its budget is asked once more, shorter; a
        // second cut one is kept with a note that it is incomplete (AGL-043
        // F6).
        let Some(first) = self.summarize(&mut base, &function.compact, budget)? else {
            return Ok(Compaction::Impossible { limit: budget });
        };
        let summary = if first.stop == Stop::Limit {
            let shorter = format!(
                "{}\n\nWrite it shorter: the summary must fit in {budget} tokens.",
                function.compact
            );
            let Some(second) = self.summarize(&mut base, &shorter, budget)? else {
                return Ok(Compaction::Impossible { limit: budget });
            };
            let text = second.text.trim();
            if second.stop == Stop::Limit && !text.is_empty() {
                format!(
                    "{text}\n\n(This summary was cut at its limit of {budget} tokens and is \
                     incomplete.)"
                )
            } else {
                text.to_owned()
            }
        } else {
            first.text.trim().to_owned()
        };
        if summary.is_empty() {
            return Ok(Compaction::Impossible { limit: budget });
        }
        let summary = summary.as_str();
        // The question that started the turn is kept after the summary
        // (F31): the summary need not repeat it, and the turn goes on from
        // the summary and the question together.
        let question = self
            .session
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                Event::User(text) => Some(text.clone()),
                _ => None,
            });
        self.session.append(Event::Compact {
            summary: summary.into(),
            question,
        })?;
        // The summary is kept in the memory directory too, not in the
        // index and not in the prompt (AGL-006, M9, M16); a failure to
        // write it does not undo the compaction.
        if let Ok((dir, _)) = self.memory.write() {
            let date = crate::session::utc_now();
            let _ = crate::memory::write_summary(
                &dir,
                &date[..10.min(date.len())],
                &self.session.id,
                summary,
            );
        }
        self.session.renew_memory_index();
        // The dock's figure is fresh at once (AGL-043 F7) and the row of the
        // compaction can name it (AGL-046, D13); a failed count does not
        // undo the compaction, the next step counts again.
        if let Ok(tokens) = self.history_tokens() {
            report.context_used(tokens);
        }
        report.compacted();
        Ok(Compaction::Done)
    }

    /// Logs a stop and ends the turn with it.
    fn stop(&mut self, reason: StopReason) -> Result<Option<TurnEnd>, Error> {
        self.session.append(Event::Stop(reason))?;
        Ok(Some(TurnEnd::Stopped(reason)))
    }
}

/// The reserve of the early compaction (AGL-038, D6): the most a step can
/// add to the history — its action, its thinking, a tool result of
/// `tools::LIMIT` bytes counted at a token per byte — and the summary.
fn early_reserve(function: &Function, context: usize) -> usize {
    function.action_tokens(context) as usize
        + function.max_think_tokens as usize
        + crate::tools::LIMIT
        + function.compact_tokens(context) as usize
}

/// How much of a tool result the summary request keeps at each end
/// (AGL-038, D5), in characters.
const SUMMARY_KEEP: usize = 1024;

/// The characters of a token in the estimate of a cut (AGL-038, D5): low,
/// so a cut removes rather more than it must and the next count fits.
const CUT_CHARS_PER_TOKEN: usize = 3;

/// Cuts the summary request `messages` (the system prompt first) by about
/// `over` tokens (AGL-038, D5): the tool results, the oldest first, to
/// their head and tail with a mark; when none is left to cut, the oldest
/// turns — from the first message after the previous summary up to the
/// next user message —, the latest turn kept. `false` when nothing more
/// can be cut.
fn cut_for_summary(messages: &mut Vec<Message>, over: usize) -> bool {
    let goal = over.saturating_mul(CUT_CHARS_PER_TOKEN).max(1);
    let mut removed = 0;
    for message in messages.iter_mut() {
        if removed >= goal {
            return true;
        }
        if let Message::Tool { content, .. } = message
            && let Some(cut) = cut_result(content)
        {
            removed += content.len() - cut.len();
            *content = cut;
        }
    }
    if removed > 0 {
        return true;
    }
    // The previous summary leads the history (`Session::history`).
    let first = match messages.get(1) {
        Some(Message::Runtime(_)) => 2,
        _ => 1,
    };
    let mut dropped = false;
    while removed < goal {
        let Some(next) = messages
            .iter()
            .skip(first + 1)
            .position(|m| matches!(m, Message::User(_)))
            .map(|i| first + 1 + i)
        else {
            break;
        };
        removed += messages
            .drain(first..next)
            .map(|m| message_chars(&m))
            .sum::<usize>();
        dropped = true;
    }
    dropped
}

/// The tool result `content` cut to its head and tail with a mark, or
/// `None` when it is short or already cut (AGL-038, D5).
fn cut_result(content: &str) -> Option<String> {
    let chars = content.chars().count();
    if chars <= 2 * SUMMARY_KEEP + 64 || content.contains("characters cut for the summary]") {
        return None;
    }
    let head: String = content.chars().take(SUMMARY_KEEP).collect();
    let tail: String = content.chars().skip(chars - SUMMARY_KEEP).collect();
    Some(format!(
        "{head}\n[{} characters cut for the summary]\n{tail}",
        chars - 2 * SUMMARY_KEEP
    ))
}

/// The characters of a message, for the estimate of a cut.
fn message_chars(message: &Message) -> usize {
    match message {
        Message::System(text) | Message::User(text) | Message::Runtime(text) => text.len(),
        Message::Assistant { thinking, text, call } => {
            thinking.as_deref().map_or(0, str::len)
                + text.as_deref().map_or(0, str::len)
                + call.as_ref().map_or(0, |call| {
                    call.args
                        .iter()
                        .map(|(name, value)| name.len() + format!("{value:?}").len())
                        .sum::<usize>()
                })
        }
        Message::Tool { content, .. } => content.len(),
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

/// The corrective nudge of a stir, as a runtime message (AGL-030, D4, D8):
/// it names the state, asks for exactly one action, and includes what was
/// wrong (`detail`); the failed output is not quoted.
fn stir_nudge(state: &str, detail: &str) -> String {
    format!(
        "Your last output was not an action of the `{state}` state ({detail}). \
         Respond with exactly one action: a call of a tool the state allows, or an \
         answer where it allows one."
    )
}

/// The system message of a step and of a compaction: the Function's
/// `system` and nothing the runtime adds (AGL-017, D3, D4). What the
/// runtime prefix means and how large an action may be (AGL-009) is in the
/// `system` of the built-in `agentLIBRE` Function, not here. Stable text, for
/// the prompt cache (D14, D19).
fn system_prompt(function: &Function, context: usize) -> String {
    function.system_at(context)
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
