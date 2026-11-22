//! The agent turn: steps of thinking and an action under the state
//! grammar until an answer or a stop (D11, D14, D19, D32, D33, D44, D45,
//! D94, D105, D108, D110, D113), and compaction of the history (D139,
//! D140).

use std::path::Path;

use crate::action::Action;
use crate::attach::Transcriber;
use crate::backend::{
    Backend, Message, Prefill, Prompt, PromptProgress, ReasoningLevel, Request, Step, StepInput,
    StepLog, Stepped, Stop, ToolCall,
};
use crate::error::Error;
use crate::function::Function;
use crate::machine::{Machine, Progress};
use crate::media;
use crate::phase::{Observer, PhaseEvent, Reports, StepRecord, Transition};
use crate::search::Searcher;
use crate::session::{CutOff, Event, INTERRUPTED, Session, StopReason};
use crate::tools::{self, ForgeEnv, Spec};

/// What a turn tells while it runs (AGL-067, F3, D3): plain data, owned,
/// so a surface can draw it where it is and a channel can carry it. An
/// event of the session as it was appended, or what is not stored.
#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    /// An event of the session, as it was appended: a visit of a state, a
    /// call with the text before it, its result, the answer, a cut-off, a
    /// write of the forge or of the memory.
    Event(Event),
    /// A step begins: its prompt is rendered, counted and read by the
    /// server until the first token (AGL-043 F2).
    Prefill,
    /// A piece of thinking as it streams.
    Thinking(String),
    /// Thinking of this step is over.
    ThinkingDone,
    /// The first token of the action, shown or not: the text of a call is
    /// not, and it is being written all the same (AGL-058, D7).
    ActionStarted,
    /// A piece of the action's text as it streams, or the whole of it at
    /// the end; never a piece of the call itself (D131).
    ActionText(String),
    /// A stir of the stirred mode: the `attempt`-th corrective retry of the
    /// step, of at most `max`, after the malformed output named by `error`
    /// (AGL-030, D4; AGL-043 F8).
    Stirred { attempt: u32, max: u32, error: String },
    /// The summary is being generated (D140).
    Compacting,
    /// A summary replaced the history (D139).
    Compacted,
    /// A compaction that started made no summary (AGL-046).
    CompactFailed,
    /// The prompt of the next step has this many tokens (D152).
    ContextUsed(usize),
    /// A generation decoded this many tokens (AGL-012).
    Decoded(usize),
    /// How the reading of a prompt goes, as the server says (AGL-046, D19).
    PromptProgress(PromptProgress),
    /// How the server read the prompt of a generation (AGL-046).
    PromptRead(Prefill),
    /// What a step's prompt is made of (AGL-046, D15).
    StepStarted(StepRecord),
    /// A transition of the FSM during the turn (AGL-046).
    Transition(Transition),
}

/// Where the reports of a turn go (AGL-067, F3, D3): a closure, or a type
/// that draws or logs them.
pub trait Sink {
    fn send(&mut self, report: Report);
}

impl<F: FnMut(Report)> Sink for F {
    fn send(&mut self, report: Report) {
        self(report)
    }
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// What the model is told of its environment and of this agl (AGL-048).
    pub about: Option<&'a str>,
    /// The daemon's holder of the plane's flows, which the `flow` tool asks;
    /// none outside a plane (AGL-118, D2, D17).
    pub flows: Option<crate::plane::flow::holder::Handle>,
}

impl Agent<'_> {
    /// Runs the turn for the user `message`, with an observer of its own.
    #[cfg(any(test, feature = "testutil"))]
    pub fn turn(
        &mut self,
        message: &str,
        sink: &mut dyn Sink,
    ) -> Result<TurnEnd, Error> {
        let mut observer = Observer::new();
        observer.attach(self.session);
        self.turn_observed(message, sink, &mut observer)
    }

    /// Runs the turn for the user `message`. A stop is logged; a server
    /// error is logged as a stop and returned. `observer` saw what came
    /// before the turn: the start of the server and the transcription
    /// (AGL-046, D10).
    pub fn turn_observed(
        &mut self,
        message: &str,
        sink: &mut dyn Sink,
        observer: &mut Observer,
    ) -> Result<TurnEnd, Error> {
        let mut reports = Reports { observer, sink };
        let end = self.run_turn(message, &mut reports);
        match &end {
            Ok(TurnEnd::Answer(_)) => {}
            Ok(TurnEnd::Stopped(reason)) => reports.end(PhaseEvent::Stop(*reason)),
            Err(_) => reports.end(PhaseEvent::Failed),
        }
        end
    }

    fn run_turn(&mut self, message: &str, report: &mut Reports<'_>) -> Result<TurnEnd, Error> {
        self.session.append(Event::User(message.into()))?;
        let unfinished = self.session.unfinished().expect("the turn has a message");
        let mut machine = Machine::resume(self.function.table(), self.function_name, &unfinished)?;
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
                    | Error::ServerCertificate { .. }
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

    fn enter(&mut self, state: &str, report: &mut Reports<'_>) -> Result<(), Error> {
        self.record(Event::State(state.into()), report)
    }

    /// Appends `event` to the session and reports it as it was appended.
    fn record(&mut self, event: Event, report: &mut Reports<'_>) -> Result<(), Error> {
        self.session.append(event.clone())?;
        report.send(Report::Event(event));
        Ok(())
    }

    /// One step; `None` when the turn goes on.
    fn step(
        &mut self,
        machine: &mut Machine,
        report: &mut Reports<'_>,
    ) -> Result<Option<TurnEnd>, Error> {
        report.send(Report::Prefill);
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
        let prompt = format.start_step(rendered, think);

        // The step must fit the context with its full generation (D32).
        let think_budget = if think {
            function.max_think_tokens as usize
        } else {
            0
        };
        // A picture is counted by its bound, not by the marker (D10).
        let prompt_tokens = self.backend.count_prompt(&step_prompt, &prompt)?
            + expanded.pictures * self.image_tokens;
        report.send(Report::ContextUsed(prompt_tokens));
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

        // The step of the backend: its thinking and its action, the action
        // parsed or the cut-off or what was wrong (AGL-067, D6). The stirred
        // mode tells a malformed one back a bounded number of times
        // (AGL-030, D4, D8).
        let state_name = state.name.clone();
        let instruction = state.instruction.clone();
        let actions = state.actions.clone();
        let mut stirs = 0u32;
        loop {
            let step_prompt = Prompt {
                messages: &messages,
                instruction: instruction.as_deref(),
                tools: &tools,
                preserve_thinking: function.preserve_thinking,
                reasoning,
                format,
            };
            let Step { stepped, log } = self.backend.step(
                &StepInput {
                    function,
                    actions: &actions,
                    prompt: &step_prompt,
                    rendered: &prompt,
                    think,
                    media: &expanded.uploads,
                    context: self.context,
                },
                report,
            );
            // Only the agent writes the session (AGL-121, D21): the step's
            // log first, also when the step failed after it (D50).
            self.write_step_log(log)?;
            match stepped? {
                Stepped::Action(action) => return self.run_action(machine, report, action),
                Stepped::CutOff(cut) => return self.cut_off(machine, report, cut),
                Stepped::Malformed(detail) => {
                    if stirs >= function.max_stirs {
                        return self.stop(StopReason::StirLimit);
                    }
                    stirs += 1;
                    self.session.append(Event::Stir {
                        attempt: stirs,
                        error: detail.clone(),
                    })?;
                    report.send(Report::Stirred {
                        attempt: stirs,
                        max: function.max_stirs,
                        error: detail.clone(),
                    });
                    // The malformed output is discarded; the retry sees the
                    // nudge as a runtime message (AGL-030, D4, D8).
                    messages.push(Message::Runtime(stir_nudge(&state_name, &detail)));
                }
            }
        }
    }

    /// The thinking and the grammar of a step, in the order they came
    /// (AGL-121, D21).
    fn write_step_log(&mut self, log: StepLog) -> Result<(), Error> {
        if let Some(thinking) = log.thinking {
            self.session.append(Event::Thinking(thinking))?;
        }
        if let Some(grammar) = log.grammar {
            self.session.record_input("grammar", &grammar)?;
        }
        Ok(())
    }

    /// The action of the step ran out of tokens (D154): it is recorded and
    /// discarded, and the machine moves on.
    fn cut_off(
        &mut self,
        machine: &mut Machine,
        report: &mut Reports<'_>,
        cut: CutOff,
    ) -> Result<Option<TurnEnd>, Error> {
        self.record(Event::CutOff(cut), report)?;
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
        report: &mut Reports<'_>,
        action: Action,
    ) -> Result<Option<TurnEnd>, Error> {
        match action {
            Action::Answer(text) => {
                self.record(Event::Answer(text.clone()), report)?;
                Ok(Some(TurnEnd::Answer(text)))
            }
            Action::Call { text, tool, args } => {
                let call = ToolCall {
                    id: self.session.next_call_id(),
                    name: tool,
                    args,
                };
                self.record(
                    Event::Call {
                        text: text.clone(),
                        call: call.clone(),
                    },
                    report,
                )?;
                let ran = tools::run(
                    &call,
                    text.as_deref(),
                    &mut tools::Reach {
                        workspace: self.workspace,
                        forge: self.forge,
                        output_tokens: self.function.output_tokens(self.context),
                        session: &mut *self.session,
                        backend: self.backend,
                        whisper: self.whisper,
                        search: self.search,
                        memory: self.memory,
                        flows: self.flows.clone(),
                    },
                );
                let result = ran.result;
                self.record(
                    Event::Result {
                        id: call.id.clone(),
                        outcome: result.outcome,
                        content: result.content.clone(),
                    },
                    report,
                )?;
                match ran.write {
                    Some(tools::Written::Memory(write)) => self.record(
                        Event::Memory {
                            name: write.name,
                            kind: write.kind,
                            text: write.text,
                        },
                        report,
                    )?,
                    Some(tools::Written::Forge(write)) => self.record(
                        Event::Forge {
                            name: write.name,
                            kind: write.kind,
                            section: write.section,
                        },
                        report,
                    )?,
                    None => {}
                }
                if crate::stop::requested() {
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

    /// Records what the prompt of a step is made of (AGL-046, D14, D15):
    /// each part in full when it changed, and the step's hash and budgets.
    #[allow(clippy::too_many_arguments)]
    fn record_step(
        &mut self,
        report: &mut Reports<'_>,
        state: &crate::function::State,
        messages: &[Message],
        tools: &[&'static Spec],
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
            // The index without its heading line.
            Some(Message::Runtime(text)) => text.split_once('\n').map_or("", |(_, index)| index),
            _ => "",
        };
        self.session.record_input("memory_index", memory)?;
        self.session
            .record_input("sampling", &format!("{:?}", function.sampling))?;
        self.session
            .record_input("model", &format!("{:?}", function.model))?;
        report.send(Report::StepStarted(StepRecord {
            state: state.name.clone(),
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
            stirred: self.backend.stirred(),
            reasoning: format!("{:?}", self.reasoning),
        }));
        Ok(())
    }

    /// The messages of a step before its state's instruction: the system
    /// prompt, the index of the workspace's memory, which is not stored
    /// (AGL-006, M27), then the history. The server's cache holds only if the
    /// next prompt continues this one, so nothing stands after the history
    /// that the next step's history would go before (AGL-047, D1).
    fn step_messages(&mut self) -> Vec<Message> {
        let mut system = system_prompt(self.function, self.context);
        if let Some(about) = self.about {
            system.push_str("\n\n");
            system.push_str(
                &about
                    .replace(crate::about::CONTEXT, &self.context.to_string())
                    .replace(
                        crate::about::SESSION_LOG,
                        &self.session.file_path().display().to_string(),
                    ),
            );
        }
        let mut messages = vec![Message::System(system)];
        let memory = self.memory;
        // The heading and the index, both marked in a plane (AGL-077, D8).
        if let Some(index) = self.session.memory_index(|| memory.index()) {
            messages.push(Message::Runtime(index));
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
                structured: self.backend.stirred().then_some(&structured),
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
    pub fn compact(&mut self, sink: &mut dyn Sink) -> Result<Compaction, Error> {
        let mut observer = Observer::new();
        observer.attach_between_turns(self.session);
        let mut reports = Reports {
            observer: &mut observer,
            sink,
        };
        let result = self.compact_history(&mut reports);
        reports.end(if result.is_ok() {
            PhaseEvent::Idle
        } else {
            PhaseEvent::Failed
        });
        result
    }

    /// The compaction inside a turn or of [`Agent::compact`].
    fn compact_history(&mut self, report: &mut Reports<'_>) -> Result<Compaction, Error> {
        let result = self.summarize_history(report)?;
        if matches!(result, Compaction::Impossible { .. }) {
            report.send(Report::CompactFailed);
        }
        Ok(result)
    }

    fn summarize_history(&mut self, report: &mut Reports<'_>) -> Result<Compaction, Error> {
        if !self.session.compactable() {
            return Ok(Compaction::Nothing);
        }
        report.send(Report::Compacting);
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
        // The question of a turn that is going on is kept after the summary
        // (F31): the summary need not repeat it, and the turn goes on from
        // the summary and the question together. Between turns it has its
        // answer, and asked again it would be answered again.
        let question = self.session.unfinished().and_then(|_| {
            self.session
                .events
                .iter()
                .rev()
                .find_map(|event| match event {
                    Event::User(text) => Some(text.clone()),
                    _ => None,
                })
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
            report.send(Report::ContextUsed(tokens));
        }
        report.send(Report::Compacted);
        Ok(Compaction::Done)
    }

    /// Logs a stop and ends the turn with it.
    fn stop(&mut self, reason: StopReason) -> Result<Option<TurnEnd>, Error> {
        self.session.append(Event::Stop(reason))?;
        Ok(Some(TurnEnd::Stopped(reason)))
    }
}

/// The reserve of the early compaction (AGL-038, D6): the most a step can
/// add to the history — its action, its thinking, a tool result at its limit
/// (AGL-049) — and the summary.
fn early_reserve(function: &Function, context: usize) -> usize {
    function.action_tokens(context) as usize
        + function.max_think_tokens as usize
        + function.output_tokens(context)
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
fn function_tools(function: &Function) -> Vec<&'static Spec> {
    tools::SPECS
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
