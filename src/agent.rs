//! The agent turn: steps of thinking and an action under the state
//! grammar until an answer or a stop (D11, D14, D19, D32, D33, D44, D45,
//! D94, D105, D108, D110, D113).

use std::path::Path;

use crate::action::{self, Action, ParseError};
use crate::backend::{Backend, Message, Prompt, Request, Stop, TOOL_DOCS, ToolCall, ToolDoc};
use crate::error::Error;
use crate::function::Function;
use crate::interrupt;
use crate::machine::{Machine, Progress};
use crate::session::{Event, INTERRUPTED, Session, StopReason};
use crate::tools::{self, ToolResult};

/// What the user sees while a turn runs.
pub trait Report {
    /// A visit of `state` begins.
    fn state(&mut self, state: &str);
    /// A piece of thinking as it streams.
    fn thinking(&mut self, piece: &str);
    /// Thinking of this step is over.
    fn thinking_done(&mut self);
    /// Text the model wrote before a call (D114).
    fn text(&mut self, text: &str);
    fn call(&mut self, call: &ToolCall);
    fn result(&mut self, call: &ToolCall, result: &ToolResult);
}

/// How a turn ended.
#[derive(Debug, PartialEq, Eq)]
pub enum TurnEnd {
    Answer(String),
    Stopped(StopReason),
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
        let mut messages = vec![Message::System(function.system.clone())];
        messages.extend(self.session.history());
        let tools = function_tools(function);
        let rendered = self.backend.render(&Prompt {
            messages: &messages,
            instruction: state.instruction.as_deref(),
            tools: &tools,
            preserve_thinking: function.preserve_thinking,
        })?;
        let mut prompt = format.start_step(rendered, state.think);

        // The step must fit the context with its full generation (D32).
        let think_budget = if state.think {
            function.max_think_tokens as usize
        } else {
            0
        };
        let needed = self.backend.count_tokens(&prompt)?
            + think_budget
            + function.max_action_tokens as usize;
        if needed > self.context {
            return self.stop(StopReason::ContextFull);
        }

        if let Some(think) = format.think.as_ref().filter(|_| state.think) {
            let close = think.close.trim();
            let call_start = format.call.begin_before.trim();
            let thinking = self.backend.generate(
                &Request {
                    prompt: &prompt,
                    grammar: None,
                    stop: &[close, call_start],
                    max_tokens: function.max_think_tokens,
                    sampling: &function.sampling,
                },
                &mut |piece| report.thinking(piece),
            )?;
            report.thinking_done();
            let text = thinking.text.trim_end();
            if !text.is_empty() {
                self.session.append(Event::Thinking(text.into()))?;
            }
            // A cut-off thinking or a call started inside it ends the
            // thinking too (D110, D113).
            prompt.push_str(text);
            prompt.push_str(&think.close);
        }

        let (grammar, root) = action::grammar(format, &state.actions);
        let generated = self.backend.generate(
            &Request {
                prompt: &prompt,
                grammar: Some(&grammar.render(&root)),
                stop: &[],
                max_tokens: function.max_action_tokens,
                sampling: &function.sampling,
            },
            &mut |_| {},
        )?;
        // Cut-off text may have been heading for a call (D113, D114).
        if generated.stop == Stop::Limit {
            return self.stop(StopReason::CutOff);
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
                let result = tools::run(self.workspace, &call.name, &call.args);
                self.session.append(Event::Result {
                    id: call.id.clone(),
                    outcome: result.outcome,
                    content: result.content.clone(),
                })?;
                report.result(&call, &result);
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

    /// Logs a stop and ends the turn with it.
    fn stop(&mut self, reason: StopReason) -> Result<Option<TurnEnd>, Error> {
        self.session.append(Event::Stop(reason))?;
        Ok(Some(TurnEnd::Stopped(reason)))
    }
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
