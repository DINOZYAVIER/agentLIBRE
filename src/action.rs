//! What the model may do in a state: the action grammar and parsing the
//! model output into an action (D7, D13, D22, D24-D26, D79-D81).

use crate::format::Format;
use crate::grammar::{Expr, Grammar, int_range};

/// The type of a parameter value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Int,
}

/// Values a parameter accepts (D41).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Constraint {
    /// Any text.
    Text,
    /// One of the values.
    OneOf(Vec<String>),
    /// An integer from `min` to `max`.
    Int { min: i64, max: i64 },
    /// Text matching a parsed `pattern`.
    Pattern(Expr),
}

/// A tool parameter as the state allows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub kind: Kind,
    pub required: bool,
    pub constraint: Constraint,
}

/// A tool as the state allows it. Parameters are in call order (D79).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    pub params: Vec<Param>,
}

/// The built-in tools with unconstrained parameters (D24-D26, D80).
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Function loading uses it from task-04")
)]
pub fn builtin_tools() -> Vec<Tool> {
    let text = |name: &str| Param {
        name: name.into(),
        kind: Kind::Text,
        required: true,
        constraint: Constraint::Text,
    };
    vec![
        Tool {
            name: "bash".into(),
            params: vec![
                text("command"),
                Param {
                    name: "timeout".into(),
                    kind: Kind::Int,
                    required: false,
                    constraint: Constraint::Int { min: 1, max: 600 },
                },
            ],
        },
        Tool {
            name: "edit".into(),
            params: vec![text("path"), text("old"), text("new")],
        },
    ]
}

/// Everything the model may do in a state: call one of `tools` or, when
/// `answer`, reply with text. At least one of them is allowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actions {
    pub tools: Vec<Tool>,
    pub answer: bool,
}

/// A parameter value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Int(i64),
}

/// What the model did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// A tool call with its arguments in call order.
    Call {
        tool: String,
        args: Vec<(String, Value)>,
    },
    /// A text answer.
    Answer(String),
}

/// Why the output is not an action. The grammar rules out anything but a
/// cut-off output; `Unexpected` means a grammar or parser bug.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The output stops in the middle of a call (D81).
    Incomplete,
    /// The output does not fit the actions at byte `offset`.
    Unexpected { offset: usize },
}

/// The grammar of `actions` in `format`, and its root.
pub fn grammar(format: &Format, actions: &Actions) -> (Grammar, Expr) {
    assert!(actions.answer || !actions.tools.is_empty(), "no actions");
    let call = &format.call;
    let mut grammar = Grammar::new();
    let mut text_value = None;
    let mut alts = Vec::new();
    for tool in &actions.tools {
        let begin = format!("{}{}{}", call.begin_before, tool.name, call.begin_after);
        let mut items = vec![Expr::literal(&begin)];
        for param in &tool.params {
            let value = match &param.constraint {
                Constraint::Text => text_value
                    .get_or_insert_with(|| grammar.text_before("value", &call.param_after))
                    .clone(),
                Constraint::OneOf(values) => Expr::one_of(values),
                Constraint::Int { min, max } => int_range(*min, *max).expect("min <= max"),
                Constraint::Pattern(expr) => expr.clone(),
            };
            let header = format!("{}{}{}", call.param_before, param.name, call.param_between);
            let item = Expr::Seq(vec![
                Expr::literal(&header),
                value,
                Expr::literal(&call.param_after),
            ]);
            items.push(if param.required {
                item
            } else {
                Expr::Repeat {
                    expr: Box::new(item),
                    min: 0,
                    max: Some(1),
                }
            });
        }
        items.push(Expr::literal(&call.end));
        alts.push(grammar.rule("call", Expr::Seq(items)));
    }
    if actions.answer {
        alts.push(grammar.text_without("answer", &call.begin_before, true));
    }
    let root = if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        Expr::Alt(alts)
    };
    (grammar, root)
}

/// Parses an output generated under [`grammar`].
pub fn parse(format: &Format, actions: &Actions, output: &str) -> Result<Action, ParseError> {
    let call = &format.call;
    let mut cursor = Cursor {
        text: output,
        pos: 0,
    };
    // The grammar keeps the start of a call out of answers.
    if actions.tools.is_empty() || !output.starts_with(&call.begin_before) {
        if actions.answer && !output.is_empty() {
            return Ok(Action::Answer(output.into()));
        }
        if actions.tools.is_empty() {
            return Err(ParseError::Incomplete);
        }
    }
    cursor.expect(&call.begin_before)?;
    let tool = cursor.choose(
        actions
            .tools
            .iter()
            .map(|tool| (format!("{}{}", tool.name, call.begin_after), tool)),
    )?;
    let mut args = Vec::new();
    for param in &tool.params {
        let header = format!("{}{}{}", call.param_before, param.name, call.param_between);
        if !param.required && !cursor.rest().starts_with(&header) {
            // A cut-off header cannot be anything else.
            if !cursor.rest().is_empty() && header.starts_with(cursor.rest()) {
                return Err(ParseError::Incomplete);
            }
            continue;
        }
        cursor.expect(&header)?;
        let text = match &param.constraint {
            Constraint::OneOf(values) => {
                let value = cursor.choose(
                    values
                        .iter()
                        .map(|value| (format!("{value}{}", call.param_after), value)),
                )?;
                value.clone()
            }
            // Text values end at the first `param_after` by grammar; the
            // others cannot contain it (D72).
            _ => {
                let Some(len) = cursor.rest().find(&call.param_after) else {
                    return Err(ParseError::Incomplete);
                };
                let value = cursor.rest()[..len].to_owned();
                cursor.pos += len + call.param_after.len();
                value
            }
        };
        let value = match param.kind {
            Kind::Text => Value::Text(text),
            Kind::Int => Value::Int(text.parse().map_err(|_| cursor.unexpected())?),
        };
        args.push((param.name.clone(), value));
    }
    cursor.expect(&call.end)?;
    if !cursor.rest().is_empty() {
        return Err(cursor.unexpected());
    }
    Ok(Action::Call {
        tool: tool.name.clone(),
        args,
    })
}

struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl Cursor<'_> {
    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn unexpected(&self) -> ParseError {
        ParseError::Unexpected { offset: self.pos }
    }

    /// Error for text that does not start with any of `expected`.
    fn mismatch<'e>(&self, mut expected: impl Iterator<Item = &'e str>) -> ParseError {
        if expected.any(|e| e.starts_with(self.rest())) {
            ParseError::Incomplete
        } else {
            self.unexpected()
        }
    }

    fn expect(&mut self, literal: &str) -> Result<(), ParseError> {
        if self.rest().starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(self.mismatch([literal].into_iter()))
        }
    }

    /// Moves past the longest of the texts that starts the rest and returns
    /// its item.
    fn choose<T>(&mut self, options: impl Iterator<Item = (String, T)>) -> Result<T, ParseError> {
        let options: Vec<(String, T)> = options.collect();
        let best = options
            .iter()
            .enumerate()
            .filter(|(_, (text, _))| self.rest().starts_with(text.as_str()))
            .max_by_key(|(_, (text, _))| text.len())
            .map(|(i, (text, _))| (i, text.len()));
        match best {
            Some((i, len)) => {
                self.pos += len;
                Ok(options.into_iter().nth(i).unwrap().1)
            }
            None => Err(self.mismatch(options.iter().map(|(text, _)| text.as_str()))),
        }
    }
}

#[cfg(test)]
mod tests;
