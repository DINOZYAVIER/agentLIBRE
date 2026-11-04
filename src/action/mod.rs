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
pub fn builtin_tools() -> Vec<Tool> {
    let text = |name: &str| Param {
        name: name.into(),
        kind: Kind::Text,
        required: true,
        constraint: Constraint::Text,
    };
    let optional = |name: &str| Param {
        name: name.into(),
        kind: Kind::Text,
        required: false,
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
        // A medium into the conversation (AGL-014, D24).
        Tool {
            name: "view".into(),
            params: vec![text("path")],
        },
        // The human's search service (AGL-019, D6). The parameters are in
        // alphabetical order, as the template shows them (see `forge`);
        // the two optional ones come first, the grammar lets them be skipped.
        Tool {
            name: "search".into(),
            params: vec![
                Param {
                    name: "language".into(),
                    kind: Kind::Text,
                    required: false,
                    constraint: Constraint::OneOf(
                        crate::search::LANGUAGES.iter().map(|l| l.to_string()).collect(),
                    ),
                },
                Param {
                    name: "limit".into(),
                    kind: Kind::Int,
                    required: false,
                    constraint: Constraint::Int {
                        min: 1,
                        max: crate::search::MAX_LIMIT,
                    },
                },
                text("query"),
                Param {
                    name: "source".into(),
                    kind: Kind::Text,
                    required: true,
                    constraint: Constraint::OneOf(
                        crate::search::SOURCES.iter().map(|s| s.to_string()).collect(),
                    ),
                },
            ],
        },
        // The memory of the workspace (AGL-006): one tool, an `action` plus
        // the fields, in alphabetical order as `forge`'s are.
        Tool {
            name: "memory".into(),
            params: vec![
                text("action"),
                optional("hook"),
                optional("name"),
                optional("new"),
                optional("old"),
                optional("text"),
                optional("title"),
            ],
        },
        // One tool, an `action` plus the per-action fields (AGL-008);
        // which fields an action needs is the tool's check. The fields
        // are in alphabetical order: the template shows the parameters
        // ordered by key, and the grammar lets an optional parameter be
        // skipped, so a call in the order the model reads them (`new`
        // before `old`) would lose the parameter that comes "too late".
        Tool {
            name: "forge".into(),
            params: vec![
                text("action"),
                optional("name"),
                optional("new"),
                optional("old"),
                optional("section"),
                optional("state"),
                optional("text"),
                optional("title"),
            ],
        },
    ]
}

/// How a tool call ended (D16, D25, D26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Fail,
    Timeout,
}

impl Outcome {
    /// The name in transition keys such as `bash.ok` (D43).
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Fail => "fail",
            Outcome::Timeout => "timeout",
        }
    }
}

/// The outcomes a built-in tool can have.
pub fn outcomes(tool: &str) -> &'static [Outcome] {
    match tool {
        "bash" => &[Outcome::Ok, Outcome::Fail, Outcome::Timeout],
        _ => &[Outcome::Ok, Outcome::Fail],
    }
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
    /// A tool call with its arguments in call order, and the text the model
    /// wrote before it, if any (D114).
    Call {
        text: Option<String>,
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
        let mut params = Vec::new();
        for param in &tool.params {
            let value = match &param.constraint {
                Constraint::Text => text_value
                    .get_or_insert_with(|| grammar.text_before("value", &call.param_after))
                    .clone(),
                Constraint::OneOf(values) => Expr::one_of(values),
                Constraint::Int { min, max } => int_range(*min, *max).expect("min <= max"),
                Constraint::Pattern(expr) => expr.clone(),
            };
            let (before, between, after) = call.param_parts(param.kind);
            let header = format!("{before}{}{between}", param.name);
            let mut item = vec![Expr::literal(&header), value];
            if !after.is_empty() {
                item.push(Expr::literal(after));
            }
            params.push((param.required, Expr::Seq(item)));
        }
        if call.separator.is_empty() {
            items.extend(following_params(&params, ""));
        } else {
            items.push(separated_params(&params, &call.separator));
        }
        items.push(Expr::literal(&call.end));
        alts.push(grammar.rule("call", Expr::Seq(items)));
    }
    let mut root = Vec::new();
    if !alts.is_empty() {
        // Text may come before a call (D114); it cannot start one.
        let before = grammar.text_without("before", &call.begin_before, false);
        let calls = if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Expr::Alt(alts)
        };
        root.push(Expr::Seq(vec![before, calls]));
    }
    if actions.answer {
        root.push(grammar.text_without("answer", &call.begin_before, true));
    }
    let root = if root.len() == 1 {
        root.pop().unwrap()
    } else {
        Expr::Alt(root)
    };
    (grammar, root)
}

/// The parameters that may come after others: each is written after
/// `separator`, an optional one may be missing.
fn following_params(params: &[(bool, Expr)], separator: &str) -> Vec<Expr> {
    params
        .iter()
        .map(|(required, item)| {
            let item = if separator.is_empty() {
                item.clone()
            } else {
                Expr::Seq(vec![Expr::literal(separator), item.clone()])
            };
            if *required {
                item
            } else {
                Expr::Repeat {
                    expr: Box::new(item),
                    min: 0,
                    max: Some(1),
                }
            }
        })
        .collect()
}

/// All the parameters of a call in a format with a separator (AGL-050): the
/// first one written has none before it, whichever optional ones are missing
/// before it.
fn separated_params(params: &[(bool, Expr)], separator: &str) -> Expr {
    let Some(((required, item), tail)) = params.split_first() else {
        return Expr::Seq(Vec::new());
    };
    let mut taken = vec![item.clone()];
    taken.extend(following_params(tail, separator));
    let taken = Expr::Seq(taken);
    if *required {
        taken
    } else {
        Expr::Alt(vec![taken, separated_params(tail, separator)])
    }
}

/// Maps the response of a structured generation to an action by its
/// structure (AGL-030, D10): the first call of `calls` is a call of the
/// state's tool, its JSON arguments checked against the tool's parameters
/// (the names, the required ones, the types, the allowed values and
/// ranges); no call and some `content` is an answer, where the state has
/// one. `Err` names what is wrong, for the stir.
pub fn from_structured(
    actions: &Actions,
    content: &str,
    calls: &[crate::backend::StructuredCall],
) -> Result<Action, String> {
    let text = Some(content.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let Some(call) = calls.first() else {
        return match text {
            Some(text) if actions.answer => Ok(Action::Answer(text)),
            Some(_) => Err("the state takes a call of a tool, not an answer".into()),
            None => Err("the output was empty".into()),
        };
    };
    let tool = actions
        .tools
        .iter()
        .find(|tool| tool.name == call.name)
        .ok_or_else(|| format!("`{}` is not a tool of this state", call.name))?;
    let arguments: serde_json::Value = if call.arguments.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&call.arguments)
            .map_err(|error| format!("the arguments of `{}` are not JSON ({error})", tool.name))?
    };
    let object = arguments
        .as_object()
        .ok_or_else(|| format!("the arguments of `{}` are not a JSON object", tool.name))?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !tool.params.iter().any(|param| &param.name == *key))
    {
        return Err(format!("`{}` has no parameter `{unknown}`", tool.name));
    }
    let mut args = Vec::new();
    for param in &tool.params {
        let Some(given) = object.get(&param.name) else {
            if param.required {
                return Err(format!("`{}` needs the parameter `{}`", tool.name, param.name));
            }
            continue;
        };
        let wrong = || {
            format!(
                "the parameter `{}` of `{}` is not {}",
                param.name,
                tool.name,
                match param.kind {
                    Kind::Text => "text",
                    Kind::Int => "an integer",
                }
            )
        };
        let value = match (param.kind, given) {
            (Kind::Text, serde_json::Value::String(text)) => Value::Text(text.clone()),
            (Kind::Text, serde_json::Value::Number(n)) => Value::Text(n.to_string()),
            (Kind::Text, serde_json::Value::Bool(b)) => Value::Text(b.to_string()),
            (Kind::Int, serde_json::Value::Number(n)) => {
                Value::Int(n.as_i64().ok_or_else(wrong)?)
            }
            (Kind::Int, serde_json::Value::String(text)) => {
                Value::Int(text.trim().parse().map_err(|_| wrong())?)
            }
            _ => return Err(wrong()),
        };
        match (&param.constraint, &value) {
            (Constraint::OneOf(values), Value::Text(text)) if !values.contains(text) => {
                return Err(format!(
                    "the parameter `{}` of `{}` is one of: {}",
                    param.name,
                    tool.name,
                    values.join(", ")
                ));
            }
            (Constraint::Int { min, max }, Value::Int(n)) if n < min || n > max => {
                return Err(format!(
                    "the parameter `{}` of `{}` is from {min} to {max}",
                    param.name, tool.name
                ));
            }
            _ => {}
        }
        args.push((param.name.clone(), value));
    }
    Ok(Action::Call {
        text,
        tool: tool.name.clone(),
        args,
    })
}

/// Parses an output generated under [`grammar`].
pub fn parse(format: &Format, actions: &Actions, output: &str) -> Result<Action, ParseError> {
    let call = &format.call;
    // The grammar keeps the start of a call out of any text, so the first
    // one starts the call (D114).
    let start = output
        .find(&call.begin_before)
        .filter(|_| !actions.tools.is_empty());
    let Some(start) = start else {
        if actions.answer && !output.is_empty() {
            return Ok(Action::Answer(output.into()));
        }
        // Without an answer, the output must reach a call.
        return Err(ParseError::Incomplete);
    };
    let text = Some(output[..start].trim())
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let mut cursor = Cursor {
        text: output,
        pos: start,
    };
    cursor.expect(&call.begin_before)?;
    let tool = cursor.choose(
        actions
            .tools
            .iter()
            .map(|tool| (format!("{}{}", tool.name, call.begin_after), tool)),
    )?;
    let mut args = Vec::new();
    for param in &tool.params {
        let (before, between, after) = call.param_parts(param.kind);
        let separator = if args.is_empty() { "" } else { call.separator.as_str() };
        let header = format!("{separator}{before}{}{between}", param.name);
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
                        .map(|value| (format!("{value}{after}"), value)),
                )?;
                value.clone()
            }
            // A bare integer ends where its digits do (AGL-050); a call
            // always goes on after it.
            _ if after.is_empty() => {
                let len = cursor
                    .rest()
                    .find(|c: char| !c.is_ascii_digit() && c != '-')
                    .ok_or(ParseError::Incomplete)?;
                let value = cursor.rest()[..len].to_owned();
                cursor.pos += len;
                value
            }
            // Text values end at the first `param_after` by grammar; the
            // others cannot contain it (D72).
            _ => {
                let Some(len) = cursor.rest().find(after) else {
                    return Err(ParseError::Incomplete);
                };
                let value = cursor.rest()[..len].to_owned();
                cursor.pos += len + after.len();
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
        text,
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
mod server_test;
#[cfg(test)]
mod tests;
