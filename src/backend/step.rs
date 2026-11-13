//! One step of the agent as a backend takes it (AGL-067, D6): the thinking
//! and the action of a step, for the prompt, the state's actions and the
//! budgets, giving the action, or the cut-off, or what was wrong with the
//! output. A completion server thinks to its mark and writes the action
//! under the grammar (AGL-050); a server that speaks the OpenAI dialect
//! gives one structured generation and the stirred mode retries a malformed
//! one (AGL-030, D4, D9, D10).

use std::cell::{Cell, RefCell};

use super::{Backend, Prompt, Request, Stop};
use crate::action::{self, Action, Actions};
use crate::agent::{Report, Sink};
use crate::error::Error;
use crate::function::Function;
use crate::session::{CutOff, Event, Session};

/// What a step asks of a backend.
pub struct StepInput<'a> {
    pub function: &'a Function,
    /// What the state allows.
    pub actions: &'a Actions,
    /// The conversation as the template renders it, the instruction last
    /// (D14, D19), for the backends that send it structured.
    pub prompt: &'a Prompt<'a>,
    /// The prompt rendered up to the first token of the step, for the
    /// backends that send it flat.
    pub rendered: &'a str,
    /// Whether the step thinks.
    pub think: bool,
    /// The bytes of the media the prompt names, base64 (AGL-014, D6).
    pub media: &'a [String],
    /// The context size of the server, in tokens.
    pub context: usize,
}

/// How a step came out.
#[derive(Debug, PartialEq)]
pub enum Stepped {
    Action(Action),
    /// The step hit its budget and was discarded (D154).
    CutOff(CutOff),
    /// The output is not an action of the state, and why (AGL-030, D4): the
    /// stirred mode tries the step again.
    Malformed(String),
}

/// The step of a completion server: the thinking to its mark, then the action
/// under the grammar, parsed (D3, D110, D113, D131, D154; AGL-050). The
/// thinking and the grammar are logged in `session` as they come.
pub(crate) fn completion_step<B: Backend + ?Sized>(
    backend: &B,
    session: &mut Session,
    input: &StepInput<'_>,
    sink: &mut dyn Sink,
) -> Result<Stepped, Error> {
    let function = input.function;
    let format = &function.format;
    let think = input.think;
    let mut prompt = input.rendered.to_owned();
    let action_budget = function.action_tokens(input.context);

    if let Some(markers) = format.think.as_ref().filter(|_| think) {
        let close = markers.close.trim();
        let call_start = format.call.begin_before.trim();
        let cell = RefCell::new(&mut *sink);
        let thinking = backend.generate_reading(
            &Request {
                prompt: &prompt,
                grammar: None,
                stop: &[close, call_start],
                max_tokens: function.max_think_tokens,
                sampling: &function.sampling,
                media: input.media,
                structured: None,
            },
            Some(&mut |progress| cell.borrow_mut().send(Report::PromptProgress(progress))),
            &mut |_| {},
            &mut |piece| cell.borrow_mut().send(Report::Thinking(piece.to_owned())),
        )?;
        sink.send(Report::ThinkingDone);
        if let Some(prefill) = thinking.prefill {
            sink.send(Report::PromptRead(prefill));
        }
        if !thinking.text.is_empty() {
            sink.send(Report::Decoded(backend.count_tokens(&thinking.text)?));
        }
        let text = thinking.text.trim_end().to_owned();
        if !text.is_empty() {
            session.append(Event::Thinking(text.clone()))?;
        }
        // A cut-off thinking or a call started inside it ends the
        // thinking too (D110, D113).
        prompt.push_str(&text);
        prompt.push_str(&markers.close);
    }

    // The grammar of the actions (D3): the action phase generates under
    // it.
    let (grammar, mut root) = action::grammar(format, input.actions);
    // Its text is known only at the end of a step that may reason in the
    // open (AGL-050).
    let open_reasoning = !think && format.reasons_in_the_open();
    if open_reasoning {
        root = action::after_its_empty_block(format, root);
    }
    let grammar_str = grammar.render(&root);
    session.record_input("grammar", &grammar_str)?;
    let mut stream = TextStream::new(&format.call.begin_before);
    let cell = RefCell::new(&mut *sink);
    let generated = backend.generate_reading(
        &Request {
            prompt: &prompt,
            grammar: Some(&grammar_str),
            stop: &[],
            max_tokens: action_budget,
            sampling: &function.sampling,
            media: input.media,
            structured: None,
        },
        Some(&mut |progress| cell.borrow_mut().send(Report::PromptProgress(progress))),
        &mut |_| {},
        &mut |piece| {
            cell.borrow_mut().send(Report::ActionStarted);
            if open_reasoning {
                return;
            }
            if let Some(text) = stream.push(piece) {
                cell.borrow_mut().send(Report::ActionText(text));
            }
        },
    )?;
    if let Some(prefill) = generated.prefill {
        sink.send(Report::PromptRead(prefill));
    }
    let action_tokens = if generated.text.is_empty() {
        0
    } else {
        backend.count_tokens(&generated.text)?
    };
    if action_tokens > 0 {
        sink.send(Report::Decoded(action_tokens));
    }
    let output = if open_reasoning && generated.stop != Stop::Limit {
        format.outside_reasoning(&generated.text)
    } else {
        generated.text.as_str().into()
    };
    if open_reasoning {
        if generated.stop != Stop::Limit && !output.starts_with(&format.call.begin_before) {
            sink.send(Report::ActionText(output.to_string()));
        }
    } else if let Some(text) = stream.finish() {
        sink.send(Report::ActionText(text));
    }
    // Cut-off text may have been heading for a call: it is discarded
    // and the model is told to split the work (D154).
    if generated.stop == Stop::Limit {
        return Ok(Stepped::CutOff(CutOff {
            limit: action_budget,
            thinking: None,
            discarded: u32::try_from(action_tokens).unwrap_or(u32::MAX),
            call: generated.text.contains(format.call.begin_before.trim()),
        }));
    }
    // The grammar allows nothing else, so the server broke it.
    let action = action::parse(format, input.actions, &output)
        .map_err(|_| Error::OutsideGrammar(generated.text.clone()))?;
    Ok(Stepped::Action(action))
}

/// The step of a server that speaks the OpenAI dialect (AGL-030, D4, D9, D10):
/// one structured generation — the server splits the thinking and the call
/// from the text —, the response mapped to an action by its structure; a
/// malformed one is told back for the agent's stir.
pub(crate) fn structured_step<B: Backend + ?Sized>(
    backend: &B,
    session: &mut Session,
    input: &StepInput<'_>,
    sink: &mut dyn Sink,
) -> Result<Stepped, Error> {
    let function = input.function;
    let think = input.think;
    let max_tokens = function.action_tokens(input.context)
        + if think { function.max_think_tokens } else { 0 };
    // The thinking and the text stream to the same sink; the thinking is
    // over at the first piece of text.
    let open = Cell::new(think);
    let cell = RefCell::new(&mut *sink);
    let generated = backend.generate_reading(
        &Request {
            prompt: "",
            grammar: None,
            stop: &[],
            max_tokens,
            sampling: &function.sampling,
            media: input.media,
            structured: Some(input.prompt),
        },
        Some(&mut |progress| cell.borrow_mut().send(Report::PromptProgress(progress))),
        &mut |piece| cell.borrow_mut().send(Report::Thinking(piece.to_owned())),
        &mut |piece| {
            if open.replace(false) {
                cell.borrow_mut().send(Report::ThinkingDone);
            }
            cell.borrow_mut().send(Report::ActionStarted);
            if !piece.is_empty() {
                cell.borrow_mut().send(Report::ActionText(piece.to_owned()));
            }
        },
    )?;
    if open.replace(false) {
        cell.borrow_mut().send(Report::ThinkingDone);
    }
    if let Some(prefill) = generated.prefill {
        sink.send(Report::PromptRead(prefill));
    }
    // The exact counts of the server's `usage` (AGL-030, D11).
    if let Some(tokens) = generated.prompt_tokens {
        sink.send(Report::ContextUsed(tokens));
    }
    let thinking_tokens = || -> Result<usize, Error> {
        match &generated.thinking {
            Some(thinking) => backend.count_tokens(thinking),
            None => Ok(0),
        }
    };
    let decoded = match generated.completion_tokens {
        Some(tokens) => tokens,
        None => backend.count_tokens(&generated.text)? + thinking_tokens()?,
    };
    if decoded > 0 {
        sink.send(Report::Decoded(decoded));
    }
    // A step that hit its budget is a miss of the budget, not of the model:
    // discarded, and the model is told to split the work (D154).
    if generated.stop == Stop::Limit {
        let tokens = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let thinking = if think {
            Some(tokens(thinking_tokens()?))
        } else {
            None
        };
        return Ok(Stepped::CutOff(CutOff {
            limit: max_tokens,
            // The thinking took its part of the one budget (AGL-043 F8).
            thinking,
            discarded: tokens(backend.count_tokens(&generated.text)?),
            call: !generated.tool_calls.is_empty()
                || generated.text.contains(function.format.call.begin_before.trim()),
        }));
    }
    match action::from_structured(input.actions, &generated.text, &generated.tool_calls) {
        Ok(action) => {
            if let Some(thinking) = generated
                .thinking
                .as_deref()
                .map(str::trim_end)
                .filter(|thinking| !thinking.is_empty())
            {
                session.append(Event::Thinking(thinking.into()))?;
            }
            Ok(Stepped::Action(action))
        }
        Err(detail) => Ok(Stepped::Malformed(detail)),
    }
}

/// Cuts the call out of the streamed action: passes the text before the
/// start of a call and holds back what may still turn into that start
/// (D131).
pub(crate) struct TextStream<'a> {
    begin: &'a str,
    held: String,
    in_call: bool,
}

impl<'a> TextStream<'a> {
    pub(crate) fn new(begin: &'a str) -> TextStream<'a> {
        TextStream {
            begin,
            held: String::new(),
            in_call: false,
        }
    }

    /// The text to show for `piece`, if any.
    pub(crate) fn push(&mut self, piece: &str) -> Option<String> {
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
    pub(crate) fn finish(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.held);
        (!self.in_call && !text.is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_stream_cuts_out_the_call() {
        const BEGIN: &str = "<tool_call>\n<function=";
        let run = |pieces: &[&str]| {
            let mut stream = TextStream::new(BEGIN);
            let mut shown = String::new();
            for piece in pieces {
                shown.extend(stream.push(piece));
            }
            shown.extend(stream.finish());
            shown
        };
        assert_eq!(
            run(&["Let me ", "look.\n\n", "<tool_call>\n<function=bash>"]),
            "Let me look.\n\n"
        );
        // The start of a call split over pieces is never shown.
        assert_eq!(run(&["a<", "tool", "_call>", "\n<function=x>"]), "a");
        // A tail that only looks like the start of a call is shown at the end.
        assert_eq!(run(&["x < y <", "tool"]), "x < y <tool");
        assert_eq!(run(&["no call"]), "no call");
        assert_eq!(run(&["<tool_call>\n<function=bash>"]), "");
        assert_eq!(run(&["é", "<tool_call>\n<fu", "nction=bash>"]), "é");
    }
}
