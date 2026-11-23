//! Model formats: how a model writes thinking and tool calls (D12, D23,
//! D77, D78, D81).

use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use crate::action::{Kind, Value};
use crate::error;
use crate::i18n::{Lang, Msg};
use crate::nix::Section;
use crate::tr;

const NAME: &str = "{name}";
const VALUE: &str = "{value}";

/// The Qwen 3.8 format, as `nix/formats/qwen3.8.nix` writes it (the test
/// `builtin_qwen_is_the_nix_one` keeps the two the same). The marks are
/// joined from parts: the model that edits this file cannot write them
/// whole.
pub fn qwen_value() -> Json {
    let think_open = concat!("<", "think", ">");
    let think_close = concat!("<", "/", "think", ">");
    let call_open = concat!("<", "tool_call", ">");
    let call_close = concat!("<", "/", "tool_call", ">");
    json!({
        "think": {
            "open": format!("{think_open}\n"),
            "close": format!("\n{think_close}\n\n"),
            "stray_close": think_close,
        },
        "call": {
            "begin": format!("{call_open}\n<function={{name}}>\n"),
            "param": "<parameter={name}>\n{value}\n</parameter>\n",
            "end": format!("</function>\n{call_close}"),
        },
    })
}

/// A model format.
#[derive(Debug, PartialEq, Eq)]
pub struct Format {
    /// How thinking is opened and closed; `None` for models without it.
    pub think: Option<Think>,
    pub call: Call,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Think {
    pub open: String,
    pub close: String,
    /// The block of a step without thinking: `open` + `close` unless the
    /// template writes it otherwise (AGL-050).
    pub empty: String,
    /// What a prompt ends in when a step without thinking takes no block at
    /// all: Gemma 4 goes on after a tool response with no new turn, and an
    /// empty block there makes it reason in the open (AGL-050).
    pub no_block_after: Option<String>,
    /// The mark a model ends its reasoning with when it reasons in the open in
    /// a step without thinking, as Gemma 4 does: what comes before it is not
    /// for the human, and the step's text is known only at its end (AGL-050).
    pub stray_close: Option<String>,
}

/// The call templates, split at their placeholders.
#[derive(Debug, PartialEq, Eq)]
pub struct Call {
    /// Text before `{name}` in `begin`: it starts every call.
    pub begin_before: String,
    /// Text after `{name}` in `begin`.
    pub begin_after: String,
    /// Text before `{name}` in `param`.
    pub param_before: String,
    /// Text between `{name}` and `{value}` in `param`.
    pub param_between: String,
    /// Text after `{value}` in `param`: it ends every value.
    pub param_after: String,
    /// The same three of an integer, from `int_param`; those of `param`
    /// without one. A model may write integers bare, with nothing after the
    /// value (AGL-050).
    pub int_before: String,
    pub int_between: String,
    pub int_after: String,
    /// Text between two parameters (AGL-050).
    pub separator: String,
    /// What the text outside a call cannot contain: where the start of a call
    /// is not enough, as with the marks of Gemma 4 that all start alike and
    /// would show up in an answer (AGL-050).
    pub no_text: Vec<String>,
    /// Whether the history holds the text before a call. Gemma's template
    /// writes it after the response to that call, so that the model reads what
    /// it had said as coming after the result, and a prompt no longer ends in
    /// the response (AGL-050).
    pub text_in_history: bool,
    pub end: String,
}

impl Call {
    /// The text before the name, between the name and the value, and after
    /// the value of a parameter of `kind`.
    pub fn param_parts(&self, kind: Kind) -> (&str, &str, &str) {
        match kind {
            Kind::Text => (&self.param_before, &self.param_between, &self.param_after),
            Kind::Int => (&self.int_before, &self.int_between, &self.int_after),
        }
    }

    /// Writes `args` as the model writes them.
    pub fn write_args(&self, out: &mut String, args: &[(String, Value)]) {
        for (i, (name, value)) in args.iter().enumerate() {
            if i > 0 {
                out.push_str(&self.separator);
            }
            let (kind, text) = match value {
                Value::Text(text) => (Kind::Text, text.clone()),
                Value::Int(n) => (Kind::Int, n.to_string()),
            };
            let (before, between, after) = self.param_parts(kind);
            out.push_str(before);
            out.push_str(name);
            out.push_str(between);
            out.push_str(&text);
            out.push_str(after);
        }
    }
}

impl Format {
    /// The Qwen 3.8 format, the default of the loose continue of a session
    /// whose Function is not found (AGL-063, D4): the format of the model
    /// the loose session was made with is unknown, and this is the common
    /// one, told in the notice.
    pub fn qwen() -> Format {
        let section = Section::top(Path::new("<qwen>"), qwen_value()).expect("a set");
        Self::from_section(section).expect("the qwen format parses")
    }

    /// Reads the evaluated format `file` (the `model.format` of a
    /// Function: `agl.formats."qwen3.8"` or a file of its own, AGL-039,
    /// D18); agl looks no format up by name.
    pub fn from_section(mut file: Section) -> Result<Format, error::Error> {
        file.check_keys(&["think", "call"])?;
        let think = match file.section("think")? {
            None => None,
            Some(mut think) => {
                think.check_keys(&["open", "close", "empty", "no_block_after", "stray_close"])?;
                let mut take = |key| {
                    let text = think.required_string(key)?;
                    if text.is_empty() {
                        return Err(template_error(&think, key, TemplateProblem::Empty));
                    }
                    Ok(text)
                };
                let open = take("open")?;
                let close = take("close")?;
                let empty = match think.string("empty")? {
                    Some(empty) if empty.is_empty() => {
                        return Err(template_error(&think, "empty", TemplateProblem::Empty));
                    }
                    Some(empty) => empty,
                    None => format!("{open}{close}"),
                };
                let no_block_after = think.string("no_block_after")?;
                let stray_close = match think.string("stray_close")? {
                    Some(mark) if mark.is_empty() => {
                        return Err(template_error(&think, "stray_close", TemplateProblem::Empty));
                    }
                    other => other,
                };
                Some(Think { open, close, empty, no_block_after, stray_close })
            }
        };

        let mut call = file.required_section("call")?;
        call.check_keys(&["begin", "param", "int_param", "separator", "no_text", "text_in_history", "end"])?;
        let begin = call.required_string("begin")?;
        let [begin_before, begin_after]: [String; 2] = split(&begin, &[NAME], &[VALUE])
            .map_err(|problem| template_error(&call, "begin", problem))?
            .try_into()
            .unwrap();
        if begin_before.is_empty() {
            return Err(template_error(
                &call,
                "begin",
                TemplateProblem::NothingBeforeName,
            ));
        }
        let param = call.required_string("param")?;
        let [param_before, param_between, param_after]: [String; 3] =
            split(&param, &[NAME, VALUE], &[])
                .map_err(|problem| template_error(&call, "param", problem))?
                .try_into()
                .unwrap();
        if param_after.is_empty() {
            return Err(template_error(
                &call,
                "param",
                TemplateProblem::NothingAfterValue,
            ));
        }
        let [int_before, int_between, int_after]: [String; 3] = match call.string("int_param")? {
            None => [param_before.clone(), param_between.clone(), param_after.clone()],
            Some(int_param) => split(&int_param, &[NAME, VALUE], &[])
                .map_err(|problem| template_error(&call, "int_param", problem))?
                .try_into()
                .unwrap(),
        };
        let separator = call.string("separator")?.unwrap_or_default();
        let no_text = match call.strings("no_text")? {
            Some(no_text) if no_text.is_empty() || no_text.iter().any(String::is_empty) => {
                return Err(template_error(&call, "no_text", TemplateProblem::Empty));
            }
            Some(no_text) => no_text,
            None => vec![begin_before.clone()],
        };
        let text_in_history = call.bool("text_in_history")?.unwrap_or(true);
        let end = call.required_string("end")?;
        split(&end, &[], &[NAME, VALUE])
            .map_err(|problem| template_error(&call, "end", problem))?;
        Ok(Format {
            think,
            call: Call {
                begin_before,
                begin_after,
                param_before,
                param_between,
                param_after,
                int_before,
                int_between,
                int_after,
                separator,
                no_text,
                text_in_history,
                end,
            },
        })
    }

    /// Whether a step without thinking may reason in the open, so that its
    /// text is known only at the end of the step (AGL-050).
    pub fn reasons_in_the_open(&self) -> bool {
        self.think.as_ref().is_some_and(|think| think.stray_close.is_some())
    }

    /// What is left of the `output` of a step without thinking when its
    /// reasoning in the open is taken out (AGL-050): a call alone, with no text
    /// before it, or the answer after the last mark. An output that holds
    /// nothing after the mark is what the model said, without the mark.
    pub fn outside_reasoning<'o>(&self, output: &'o str) -> std::borrow::Cow<'o, str> {
        let Some(mark) = self.think.as_ref().and_then(|think| think.stray_close.as_deref()) else {
            return output.into();
        };
        if let Some(start) = output.find(&self.call.begin_before) {
            return output[start..].into();
        }
        let Some(at) = output.rfind(mark) else {
            return output.into();
        };
        let answer = output[at + mark.len()..].trim_start();
        if answer.is_empty() {
            output.replace(mark, "").trim().to_owned().into()
        } else {
            answer.into()
        }
    }

    /// Ends a rendered `prompt` so that the model thinks or not (D94):
    /// with thinking, the prompt ends in `think.open`; without, in an empty
    /// block `think.open` + `think.close`. A `think.open` the template
    /// already added, or an empty block the template prefilled for
    /// `enable_thinking = false` (AGL-013), is replaced. Formats without
    /// `think` leave the prompt as it is.
    pub fn start_step(&self, mut prompt: String, think: bool) -> String {
        let Some(Think { open, empty, no_block_after, .. }) = &self.think else {
            return prompt;
        };
        if prompt.ends_with(empty.as_str()) {
            prompt.truncate(prompt.len() - empty.len());
        } else if prompt.ends_with(open.as_str()) {
            prompt.truncate(prompt.len() - open.len());
        }
        if think {
            prompt.push_str(open);
        } else if !no_block_after.as_ref().is_some_and(|end| prompt.ends_with(end.as_str())) {
            prompt.push_str(empty);
        }
        prompt
    }

    /// The first character that ends a value. A `pattern` that may produce
    /// it is rejected (D72).
    pub fn value_end_start(&self) -> char {
        self.call.param_after.chars().next().unwrap()
    }
}

/// What is wrong with a format template (D81).
#[derive(Debug, PartialEq, Eq)]
pub enum TemplateProblem {
    /// A placeholder the template needs is absent.
    Missing(&'static str),
    /// A placeholder appears more than once.
    Repeated(&'static str),
    /// A placeholder that does not belong in this template.
    Unexpected(&'static str),
    /// `{value}` comes before `{name}`.
    ValueBeforeName,
    /// `begin` has no text before `{name}`, so answers cannot be told
    /// apart from calls.
    NothingBeforeName,
    /// `param` has no text after `{value}`, so values have no end.
    NothingAfterValue,
    /// A `think` string is empty.
    Empty,
}

/// A format template that cannot be read (D81). Its own type, wrapped by
/// `crate::error::Error` (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// A template or string of a format is unusable (D81).
    Template { path: PathBuf, key: String, problem: TemplateProblem },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        let Self::Template { path, key, problem } = self;
        let problem = match problem {
            TemplateProblem::Missing(p) => tr!(lang, TEMPLATE_MISSING, p = p),
            TemplateProblem::Repeated(p) => tr!(lang, TEMPLATE_REPEATED, p = p),
            TemplateProblem::Unexpected(p) => tr!(lang, TEMPLATE_UNEXPECTED, p = p),
            TemplateProblem::ValueBeforeName => tr!(lang, TEMPLATE_VALUE_BEFORE_NAME).to_string(),
            TemplateProblem::NothingBeforeName => {
                tr!(lang, TEMPLATE_NOTHING_BEFORE_NAME).to_string()
            }
            TemplateProblem::NothingAfterValue => {
                tr!(lang, TEMPLATE_NOTHING_AFTER_VALUE).to_string()
            }
            TemplateProblem::Empty => tr!(lang, TEMPLATE_EMPTY).to_string(),
        };
        tr!(
            lang,
            crate::i18n::m::PROBLEM_IN_PATH,
            path = path.display(),
            key = key,
            problem = problem
        )
    }
}

/// `Error::Template`'s problems, and the shared frame `PROBLEM_IN_PATH` of
/// `crate::i18n`, also used by `Error::Function` (AGL-121, D66).
const TEMPLATE_MISSING: Msg = Msg {
    en: "needs {p}",
    uk: "потрібен {p}",
};
const TEMPLATE_REPEATED: Msg = Msg {
    en: "{p} appears more than once",
    uk: "{p} трапляється більше одного разу",
};
const TEMPLATE_UNEXPECTED: Msg = Msg {
    en: "{p} does not belong here",
    uk: "{p} тут не дозволений",
};
const TEMPLATE_VALUE_BEFORE_NAME: Msg = Msg {
    en: "{value} comes before {name}",
    uk: "{value} стоїть перед {name}",
};
const TEMPLATE_NOTHING_BEFORE_NAME: Msg = Msg {
    en: "needs text before {name}, or answers cannot be told apart from calls",
    uk: "потрібен текст перед {name}, інакше відповідь не відрізнити від виклику",
};
const TEMPLATE_NOTHING_AFTER_VALUE: Msg = Msg {
    en: "needs text after {value}, or a value has no end",
    uk: "потрібен текст після {value}, інакше значення не має кінця",
};
const TEMPLATE_EMPTY: Msg = Msg {
    en: "must not be empty",
    uk: "не може бути порожнім",
};

fn template_error(section: &Section, key: &str, problem: TemplateProblem) -> error::Error {
    Error::Template { path: section.path().into(), key: section.key(key), problem }.into()
}

/// Splits `template` at each of `placeholders`, which must appear exactly
/// once and in order. None of `forbidden` may appear.
fn split(
    template: &str,
    placeholders: &[&'static str],
    forbidden: &[&'static str],
) -> Result<Vec<String>, TemplateProblem> {
    if let Some(&placeholder) = forbidden.iter().find(|p| template.contains(**p)) {
        return Err(TemplateProblem::Unexpected(placeholder));
    }
    let mut positions = Vec::new();
    for &placeholder in placeholders {
        let count = template.matches(placeholder).count();
        if count == 0 {
            return Err(TemplateProblem::Missing(placeholder));
        }
        if count > 1 {
            return Err(TemplateProblem::Repeated(placeholder));
        }
        positions.push((template.find(placeholder).unwrap(), placeholder.len()));
    }
    if positions.windows(2).any(|pair| pair[0].0 > pair[1].0) {
        return Err(TemplateProblem::ValueBeforeName);
    }
    let mut parts = Vec::new();
    let mut start = 0;
    for (position, len) in positions {
        parts.push(template[start..position].to_owned());
        start = position + len;
    }
    parts.push(template[start..].to_owned());
    Ok(parts)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::path::Path;

    /// The built-in Qwen3.8 format.
    pub fn qwen() -> Format {
        parse(qwen_value()).unwrap()
    }


    /// The Gemma 4 format, as `nix/formats/gemma4.nix` writes it (the test
    /// `builtin_gemma_is_the_nix_one` keeps the two the same).
    pub fn gemma_value() -> Value {
        json!({
            "think": {
                "open": "<|channel>thought\n",
                "close": "\n<channel|>",
                "empty": "<|channel>thought\n<channel|>",
                "no_block_after": "<tool_response|>",
                "stray_close": "<channel|>",
            },
            "call": {
                "begin": "<|tool_call>call:{name}{",
                "param": "{name}:<|\"|>{value}<|\"|>",
                "int_param": "{name}:{value}",
                "separator": ",",
                "no_text": ["<|", "<tool_call|>", "<tool_response|>", "<turn|>"],
                "text_in_history": false,
                "end": "}<tool_call|>",
            },
        })
    }

    /// The built-in Gemma 4 format.
    pub fn gemma() -> Format {
        parse(gemma_value()).unwrap()
    }

    fn parse(value: Value) -> Result<Format, error::Error> {
        Format::from_section(Section::top(Path::new("f.nix"), value).unwrap())
    }

    fn problem(value: Value) -> (String, TemplateProblem) {
        match parse(value) {
            Err(error::Error::Format(Error::Template { key, problem, .. })) => (key, problem),
            other => panic!("expected a template error, got {other:?}"),
        }
    }

    fn call() -> Value {
        json!({ "begin": "<c {name}>", "param": "<p {name}>{value}</p>", "end": "</c>" })
    }

    #[test]
    fn builtin_qwen_is_the_nix_one() {
        let evaluated = crate::nix::eval_with_agl(Path::new("<built-in>"), "agl.formats.\"qwen3.8\"").unwrap();
        assert_eq!(evaluated, qwen_value());
    }

    #[test]
    fn the_reasoning_in_the_open_is_taken_out() {
        let format = gemma();
        let cut = |output: &str| format.outside_reasoning(output).into_owned();
        assert!(format.reasons_in_the_open());
        assert_eq!(cut("There are 9."), "There are 9.");
        assert_eq!(cut("The user asked.\n<channel|>There are 9."), "There are 9.");
        assert_eq!(cut("thought\n<channel|>There are 9."), "There are 9.");
        assert_eq!(cut("a<channel|>b<channel|>\nc"), "c");
        // Nothing after the mark: what the model said is all there is.
        assert_eq!(cut("There are 9.\n<channel|>"), "There are 9.");
        // The text before a call goes; a value of the call keeps the mark.
        let call = "<|tool_call>call:bash{command:<|\"|>grep '<channel|>' f<|\"|>}<tool_call|>";
        assert_eq!(cut(&format!("I will look.<channel|>{call}")), call);
        assert_eq!(cut(&format!("I will look.{call}")), call);
        // A format with no such mark changes nothing.
        let plain = parse(json!({ "call": self::call() })).unwrap();
        assert!(!plain.reasons_in_the_open());
        assert_eq!(plain.outside_reasoning("a<channel|>b").into_owned(), "a<channel|>b");
        // Qwen writes its answer, its mark and its answer again (AGL-058, D8).
        assert_eq!(qwen().outside_reasoning("Paris.\n</think>\n\nParis.").into_owned(), "Paris.");
    }

    #[test]
    fn builtin_gemma_is_the_nix_one() {
        let evaluated = crate::nix::eval_with_agl(Path::new("<built-in>"), "agl.formats.gemma4").unwrap();
        assert_eq!(evaluated, gemma_value());
    }

    #[test]
    fn gemma_writes_a_call_as_its_template_does() {
        let format = gemma();
        let mut out = String::new();
        format.call.write_args(
            &mut out,
            &[
                ("command".into(), crate::action::Value::Text("ls -a".into())),
                ("timeout".into(), crate::action::Value::Int(5)),
            ],
        );
        assert_eq!(out, "command:<|\"|>ls -a<|\"|>,timeout:5");
        assert_eq!(format.call.begin_before, "<|tool_call>call:");
        assert_eq!(format.call.begin_after, "{");
        assert_eq!(format.call.end, "}<tool_call|>");
    }

    #[test]
    fn gemma_starts_a_step_as_its_template_does() {
        let format = gemma();
        let base = "<|turn>model\n";
        let open = "<|turn>model\n<|channel>thought\n";
        let empty = "<|turn>model\n<|channel>thought\n<channel|>";
        // The template opens thinking after a tool response, or fills an
        // empty block without thinking; the first step has neither.
        for rendered in [base, open, empty] {
            assert_eq!(format.start_step(rendered.into(), true), open);
            assert_eq!(format.start_step(rendered.into(), false), empty);
        }
        // After a tool response the model goes on: thinking opens there, and
        // no thinking takes no block.
        let response = "<tool_response|>";
        assert_eq!(format.start_step(response.into(), false), response);
        let opened = "<tool_response|><|channel>thought\n";
        assert_eq!(format.start_step(opened.into(), true), opened);
        assert_eq!(format.start_step(opened.into(), false), response);
        assert_eq!(format.start_step(response.into(), true), opened);
    }

    #[test]
    fn builtin_qwen() {
        let format = qwen();
        assert_eq!(
            format.think,
            Some(Think {
                open: "<think>\n".into(),
                close: "\n</think>\n\n".into(),
                empty: "<think>\n\n</think>\n\n".into(),
                no_block_after: None,
                stray_close: Some("</think>".into()),
            })
        );
        assert_eq!(
            format.call,
            Call {
                begin_before: "<tool_call>\n<function=".into(),
                begin_after: ">\n".into(),
                param_before: "<parameter=".into(),
                param_between: ">\n".into(),
                param_after: "\n</parameter>\n".into(),
                int_before: "<parameter=".into(),
                int_between: ">\n".into(),
                int_after: "\n</parameter>\n".into(),
                separator: String::new(),
                no_text: vec!["<tool_call>\n<function=".into()],
                text_in_history: true,
                end: "</function>\n</tool_call>".into(),
            }
        );
        assert_eq!(format.value_end_start(), '\n');
    }

    #[test]
    fn start_step() {
        let format = qwen();
        let rendered = "<|im_start|>assistant\n<think>\n";
        let base = "<|im_start|>assistant\n";
        assert_eq!(format.start_step(rendered.into(), true), rendered);
        assert_eq!(
            format.start_step(rendered.into(), false),
            "<|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
        assert_eq!(format.start_step(base.into(), true), rendered);
        let plain = parse(json!({ "call": call() })).unwrap();
        assert_eq!(plain.start_step(base.into(), true), base);
        // A render with `enable_thinking = false` ends in an empty
        // block; the state's own block replaces it (AGL-013).
        let think = format.think.as_ref().unwrap();
        let empty_block = format!("{}{}", think.open, think.close);
        let off_rendered = format!("{base}{empty_block}");
        assert_eq!(format.start_step(off_rendered.clone(), true), rendered);
        assert_eq!(
            format.start_step(off_rendered, false),
            format.start_step(rendered.into(), false)
        );
    }

    #[test]
    fn think_is_optional() {
        assert_eq!(parse(json!({ "call": call() })).unwrap().think, None);
    }

    #[test]
    fn keys_are_checked() {
        assert!(matches!(
            parse(json!({ "name": "x", "call": call() })),
            Err(error::Error::UnknownKey { key, .. }) if key == "name"
        ));
        assert!(matches!(
            parse(json!({ "call": { "begin": "<c {name}>", "end": "</c>" } })),
            Err(error::Error::MissingKey { key, .. }) if key == "call.param"
        ));
        assert!(matches!(
            parse(json!({ "think": { "open": "<t>" }, "call": call() })),
            Err(error::Error::MissingKey { key, .. }) if key == "think.close"
        ));
        assert!(matches!(parse(json!({})), Err(error::Error::MissingKey { key, .. }) if key == "call"));
    }

    #[test]
    fn templates_are_checked() {
        use TemplateProblem::*;
        let call = |begin: &str, param: &str, end: &str| {
            json!({ "call": { "begin": begin, "param": param, "end": end } })
        };
        let ok_param = "<p {name}>{value}</p>";
        assert_eq!(
            problem(call("<c>", ok_param, "</c>")),
            ("call.begin".into(), Missing("{name}"))
        );
        assert_eq!(
            problem(call("<c {name} {name}>", ok_param, "</c>")),
            ("call.begin".into(), Repeated("{name}"))
        );
        assert_eq!(
            problem(call("<c {name} {value}>", ok_param, "</c>")),
            ("call.begin".into(), Unexpected("{value}"))
        );
        assert_eq!(
            problem(call("{name}>", ok_param, "</c>")),
            ("call.begin".into(), NothingBeforeName)
        );
        assert_eq!(
            problem(call("<c {name}>", "<p {value}>{name}</p>", "</c>")),
            ("call.param".into(), ValueBeforeName)
        );
        assert_eq!(
            problem(call("<c {name}>", "<p {name}>", "</c>")),
            ("call.param".into(), Missing("{value}"))
        );
        assert_eq!(
            problem(call("<c {name}>", "<p {name}>{value}", "</c>")),
            ("call.param".into(), NothingAfterValue)
        );
        assert_eq!(
            problem(call("<c {name}>", ok_param, "</c {name}>")),
            ("call.end".into(), Unexpected("{name}"))
        );
        assert_eq!(
            problem(json!({ "call": {
                "begin": "<c {name}>", "param": ok_param, "int_param": "{name}", "end": "</c>",
            } })),
            ("call.int_param".into(), Missing("{value}"))
        );
        assert_eq!(
            problem(json!({ "think": { "open": "", "close": "x" }, "call": super::tests::call() })),
            ("think.open".into(), Empty)
        );
    }

    #[test]
    fn literal_braces_are_text() {
        let format = parse(json!({ "call": {
            "begin": "{\"name\": \"{name}\", \"arguments\": {",
            "param": "\"{name}\": \"{value}\", ",
            "end": "}}",
        } }))
        .unwrap();
        assert_eq!(format.call.begin_before, "{\"name\": \"");
        assert_eq!(format.call.end, "}}");
    }
}
