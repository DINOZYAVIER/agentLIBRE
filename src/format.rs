//! Model formats: how a model writes thinking and tool calls (D12, D23,
//! D77, D78, D81).

use std::path::{Path, PathBuf};

use toml::Table;

use crate::error::{Error, TemplateProblem};
use crate::tomlfile::{self, Section};

/// Formats built into the binary; a user file of the same name replaces
/// one (D39).
const BUILTIN: &[(&str, &str)] = &[("qwen3.8", include_str!("builtin/formats/qwen3.8.toml"))];

const NAME: &str = "{name}";
const VALUE: &str = "{value}";

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
    pub end: String,
}

impl Format {
    /// Loads format `name` from `dir/<name>.toml` or, when that file does
    /// not exist, from the built-in formats.
    pub fn load(dir: &Path, name: &str) -> Result<Format, Error> {
        tomlfile::check_name(name)?;
        let path = dir.join(format!("{name}.toml"));
        if let Some(table) = tomlfile::read(&path)? {
            return Format::from_table(&path, table);
        }
        let Some((_, text)) = BUILTIN.iter().find(|(builtin, _)| *builtin == name) else {
            return Err(Error::FormatNotFound {
                name: name.into(),
                path,
            });
        };
        let path = PathBuf::from(format!("<built-in>/formats/{name}.toml"));
        Format::from_table(&path, tomlfile::parse(&path, text)?)
    }

    fn from_table(path: &Path, table: Table) -> Result<Format, Error> {
        let mut file = Section::new(path, table);
        file.check_keys(&["think", "call"])?;
        let think = match file.section("think")? {
            None => None,
            Some(mut think) => {
                think.check_keys(&["open", "close"])?;
                let mut take = |key| {
                    let text = think.required_string(key)?;
                    if text.is_empty() {
                        return Err(template_error(&think, key, TemplateProblem::Empty));
                    }
                    Ok(text)
                };
                let open = take("open")?;
                let close = take("close")?;
                Some(Think { open, close })
            }
        };

        let mut call = file.required_section("call")?;
        call.check_keys(&["begin", "param", "end"])?;
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
                end,
            },
        })
    }

    /// Ends a rendered `prompt` so that the model thinks or not (D94):
    /// with thinking, the prompt ends in `think.open`; without, in an empty
    /// block `think.open` + `think.close`. A `think.open` the template
    /// already added, or an empty block the template prefilled for
    /// `enable_thinking = false` (AGL-013), is replaced. Formats without
    /// `think` leave the prompt as it is.
    pub fn start_step(&self, mut prompt: String, think: bool) -> String {
        let Some(Think { open, close }) = &self.think else {
            return prompt;
        };
        let empty_block = format!("{open}{close}");
        if prompt.ends_with(empty_block.as_str()) {
            prompt.truncate(prompt.len() - empty_block.len());
        } else if prompt.ends_with(open.as_str()) {
            prompt.truncate(prompt.len() - open.len());
        }
        prompt.push_str(open);
        if !think {
            prompt.push_str(close);
        }
        prompt
    }

    /// The first character that ends a value. A `pattern` that may produce
    /// it is rejected (D72).
    pub fn value_end_start(&self) -> char {
        self.call.param_after.chars().next().unwrap()
    }
}

fn template_error(section: &Section, key: &str, problem: TemplateProblem) -> Error {
    Error::Template {
        path: section.path().into(),
        key: section.key(key),
        problem,
    }
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
    use std::fs;

    /// The built-in Qwen3.8 format.
    pub fn qwen() -> Format {
        Format::load(Path::new("/nonexistent"), "qwen3.8").unwrap()
    }

    fn parse(text: &str) -> Result<Format, Error> {
        let path = Path::new("f.toml");
        Format::from_table(path, tomlfile::parse(path, text).unwrap())
    }

    fn problem(text: &str) -> (String, TemplateProblem) {
        match parse(text) {
            Err(Error::Template { key, problem, .. }) => (key, problem),
            other => panic!("expected a template error, got {other:?}"),
        }
    }

    const CALL: &str =
        "[call]\nbegin = \"<c {name}>\"\nparam = \"<p {name}>{value}</p>\"\nend = \"</c>\"\n";

    #[test]
    fn builtin_qwen() {
        let format = qwen();
        assert_eq!(
            format.think,
            Some(Think {
                open: "<think>\n".into(),
                close: "\n</think>\n\n".into(),
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
        let plain = parse(CALL).unwrap();
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
    fn user_file_replaces_builtin() {
        let dir = std::env::temp_dir().join(format!("agentLIBRE-format-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("qwen3.8.toml"), CALL).unwrap();
        fs::write(dir.join("mine.toml"), CALL).unwrap();
        let qwen = Format::load(&dir, "qwen3.8").unwrap();
        let mine = Format::load(&dir, "mine").unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(qwen.think, None);
        assert_eq!(qwen.call.begin_before, "<c ");
        assert_eq!(mine, qwen);
    }

    #[test]
    fn missing_and_invalid_names() {
        assert!(matches!(
            Format::load(Path::new("/nonexistent"), "gemma"),
            Err(Error::FormatNotFound { name, .. }) if name == "gemma"
        ));
        for name in ["", "../qwen3.8", "a/b", ".hidden"] {
            assert!(matches!(
                Format::load(Path::new("/nonexistent"), name),
                Err(Error::InvalidName(_))
            ));
        }
    }

    #[test]
    fn think_is_optional() {
        assert_eq!(parse(CALL).unwrap().think, None);
    }

    #[test]
    fn keys_are_checked() {
        assert!(matches!(
            parse(&format!("name = \"x\"\n{CALL}")),
            Err(Error::UnknownKey { key, .. }) if key == "name"
        ));
        assert!(matches!(
            parse("[call]\nbegin = \"<c {name}>\"\nend = \"</c>\""),
            Err(Error::MissingKey { key, .. }) if key == "call.param"
        ));
        assert!(matches!(
            parse("think = { open = \"<t>\" }\n"),
            Err(Error::MissingKey { key, .. }) if key == "think.close"
        ));
        assert!(matches!(parse(""), Err(Error::MissingKey { key, .. }) if key == "call"));
    }

    #[test]
    fn templates_are_checked() {
        use TemplateProblem::*;
        let call = |begin: &str, param: &str, end: &str| {
            format!("[call]\nbegin = {begin:?}\nparam = {param:?}\nend = {end:?}\n")
        };
        let ok_param = "<p {name}>{value}</p>";
        assert_eq!(
            problem(&call("<c>", ok_param, "</c>")),
            ("call.begin".into(), Missing("{name}"))
        );
        assert_eq!(
            problem(&call("<c {name} {name}>", ok_param, "</c>")),
            ("call.begin".into(), Repeated("{name}"))
        );
        assert_eq!(
            problem(&call("<c {name} {value}>", ok_param, "</c>")),
            ("call.begin".into(), Unexpected("{value}"))
        );
        assert_eq!(
            problem(&call("{name}>", ok_param, "</c>")),
            ("call.begin".into(), NothingBeforeName)
        );
        assert_eq!(
            problem(&call("<c {name}>", "<p {value}>{name}</p>", "</c>")),
            ("call.param".into(), ValueBeforeName)
        );
        assert_eq!(
            problem(&call("<c {name}>", "<p {name}>", "</c>")),
            ("call.param".into(), Missing("{value}"))
        );
        assert_eq!(
            problem(&call("<c {name}>", "<p {name}>{value}", "</c>")),
            ("call.param".into(), NothingAfterValue)
        );
        assert_eq!(
            problem(&call("<c {name}>", ok_param, "</c {name}>")),
            ("call.end".into(), Unexpected("{name}"))
        );
        assert_eq!(
            problem(&format!("think = {{ open = \"\", close = \"x\" }}\n{CALL}")),
            ("think.open".into(), Empty)
        );
    }

    #[test]
    fn literal_braces_are_text() {
        let format = parse(&format!(
            "[call]\nbegin = {:?}\nparam = {:?}\nend = \"}}}}\"\n",
            "{\"name\": \"{name}\", \"arguments\": {", "\"{name}\": \"{value}\", "
        ))
        .unwrap();
        assert_eq!(format.call.begin_before, "{\"name\": \"");
        assert_eq!(format.call.end, "}}");
    }
}
