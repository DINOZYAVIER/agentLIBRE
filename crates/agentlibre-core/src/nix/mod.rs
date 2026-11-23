//! Nix as the language of the Functions and the settings (AGL-039, D6-D13,
//! D38, D39): the evaluation of a file by the system's `nix-instantiate`,
//! the JSON it gives, and the writing of a value back as Nix.

pub mod section;

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::error;
use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

pub use section::Section;

/// The variable of agl's `nix/` directory (AGL-039, D7, D34).
pub const NIX_VAR: &str = "AGENTLIBRE_NIX";

/// The variable of the directory of the Functions, the repository
/// agentLIBRE-functions (AGL-053, D6).
pub const FUNCTIONS_VAR: &str = "AGENTLIBRE_FUNCTIONS";

/// The evaluator, the system's, from `PATH` (AGL-039, D38, D39).
const EVALUATOR: &str = "nix-instantiate";

/// What the evaluation throws when a file is not a function (D8); agl
/// recognizes it in the evaluator's message.
const NOT_A_FUNCTION: &str = "agentLIBRE: the file is not a function of { agl, ... }";

/// What can go wrong evaluating Nix (AGL-039, D6-D13, D38; AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// `AGENTLIBRE_NIX` is not set to an absolute directory (AGL-039, D7).
    DirMissing,
    /// `AGENTLIBRE_FUNCTIONS` is not set to an absolute directory (AGL-053, D6).
    FunctionsDirMissing,
    /// The system's `nix-instantiate` cannot be run (AGL-039, D38).
    NotFound { source: io::Error },
    /// The evaluation of a Nix file failed; `detail` is Nix's message.
    Eval { path: PathBuf, detail: String },
    /// A Function or settings file is not a function of `{ agl, ... }`
    /// (AGL-039, D8).
    NotAFunction { path: PathBuf },
    /// A settings file has a key outside `allowed` (D60).
    UnknownKey {
        path: PathBuf,
        key: String,
        allowed: Vec<&'static str>,
    },
    /// A required key is missing.
    MissingKey { path: PathBuf, key: String },
    /// A settings value has the wrong type.
    WrongType {
        path: PathBuf,
        key: String,
        expected: Expected,
    },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            // `TOOLS_DIR_MISSING` is the text of
            // `programs::Error::ToolsDirMissing`, shared (AGL-121, D66, D75).
            Self::DirMissing => tr!(lang, agentlibre_common::programs::TOOLS_DIR_MISSING, var = NIX_VAR),
            Self::FunctionsDirMissing => {
                tr!(lang, agentlibre_common::programs::TOOLS_DIR_MISSING, var = FUNCTIONS_VAR)
            }
            Self::NotFound { source } => tr!(lang, NIX_NOT_FOUND, source = source),
            Self::Eval { path, detail } => {
                tr!(lang, NIX_EVAL, path = path.display(), detail = detail)
            }
            Self::NotAFunction { path } => {
                tr!(lang, NIX_NOT_A_FUNCTION, path = path.display())
            }
            Self::UnknownKey { path, key, allowed } => {
                tr!(
                    lang,
                    UNKNOWN_KEY,
                    path = path.display(),
                    key = key,
                    allowed = allowed.join(", ")
                )
            }
            Self::MissingKey { path, key } => {
                tr!(lang, MISSING_KEY, path = path.display(), key = key)
            }
            Self::WrongType {
                path,
                key,
                expected,
            } => {
                tr!(
                    lang,
                    WRONG_TYPE,
                    path = path.display(),
                    key = key,
                    expected = match expected {
                        Expected::String => tr!(lang, EXPECTED_STRING),
                        Expected::Table => tr!(lang, EXPECTED_TABLE),
                        Expected::Bool => tr!(lang, EXPECTED_BOOL),
                        Expected::Integer => tr!(lang, EXPECTED_INTEGER),
                        Expected::Number => tr!(lang, EXPECTED_NUMBER),
                        Expected::Strings => tr!(lang, EXPECTED_STRINGS),
                        Expected::Tables => tr!(lang, EXPECTED_TABLES),
                    }
                )
            }
        }
    }
}

/// `Error::NotFound`.
const NIX_NOT_FOUND: Msg = Msg {
    en: "cannot run nix-instantiate: {source} (agl needs Nix)",
    uk: "не вдалося запустити nix-instantiate: {source} (agl потребує Nix)",
};

/// `Error::Eval`.
const NIX_EVAL: Msg = Msg {
    en: "{path} does not evaluate:\n{detail}",
    uk: "{path} не обчислюється:\n{detail}",
};

/// `Error::NotAFunction`.
const NIX_NOT_A_FUNCTION: Msg = Msg {
    en: "{path} is not a function of {{ agl, ... }}: write it as `{{ agl, ... }}: {{ … }}`",
    uk: "{path} не є функцією від {{ agl, ... }}: пиши його як `{{ agl, ... }}: {{ … }}`",
};

/// The type a key of a Nix file requires.
#[derive(Debug)]
pub enum Expected {
    String,
    Table,
    Bool,
    Integer,
    Number,
    Strings,
    /// A list of sets (AGL-089).
    Tables,
}

/// `Error::UnknownKey`.
const UNKNOWN_KEY: Msg = Msg {
    en: "unknown key `{key}` in {path}; allowed: {allowed}",
    uk: "невідомий ключ `{key}` у {path}; дозволені: {allowed}",
};

/// `Error::MissingKey`.
const MISSING_KEY: Msg = Msg {
    en: "`{key}` is missing in {path}",
    uk: "у {path} немає `{key}`",
};

/// `Error::WrongType`.
const WRONG_TYPE: Msg = Msg {
    en: "`{key}` in {path} must be {expected}",
    uk: "`{key}` у {path} має бути {expected}",
};

/// What a value must be, for `WRONG_TYPE`.
const EXPECTED_STRING: Msg = Msg {
    en: "a string",
    uk: "рядком",
};

/// What a value must be, for `WRONG_TYPE`.
const EXPECTED_TABLE: Msg = Msg {
    en: "a table",
    uk: "таблицею",
};

/// What a value must be, for `WRONG_TYPE`.
const EXPECTED_BOOL: Msg = Msg {
    en: "true or false",
    uk: "true або false",
};

/// What a value must be, for `WRONG_TYPE`.
const EXPECTED_INTEGER: Msg = Msg {
    en: "an integer",
    uk: "цілим числом",
};

/// What a value must be, for `WRONG_TYPE`.
const EXPECTED_NUMBER: Msg = Msg {
    en: "a number",
    uk: "числом",
};

/// What a value must be, for `WRONG_TYPE`.
const EXPECTED_STRINGS: Msg = Msg {
    en: "an array of strings",
    uk: "масивом рядків",
};

/// `Expected::Tables` (AGL-089).
const EXPECTED_TABLES: Msg = Msg {
    en: "an array of attribute sets",
    uk: "масивом наборів атрибутів",
};

/// agl's `nix/` directory: the built-in formats, policies and models, the
/// `agl` argument of every file (AGL-039, D7, D35).
pub fn nix_dir() -> Result<PathBuf, Error> {
    std::env::var_os(NIX_VAR)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or(Error::DirMissing)
}

/// The directory of the Functions behind `agl.functions` (AGL-053, D6).
#[cfg(not(any(test, feature = "testutil")))]
pub fn functions_dir() -> Result<PathBuf, Error> {
    std::env::var_os(FUNCTIONS_VAR)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or(Error::FunctionsDirMissing)
}

/// The tests' Functions, so that a change in the repository of the Functions
/// does not break them (AGL-053, D8).
#[cfg(any(test, feature = "testutil"))]
pub fn functions_dir() -> Result<PathBuf, Error> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/function/fixtures"))
}

/// The `agl` argument, as a Nix expression.
fn agl_expr() -> Result<String, Error> {
    Ok(format!(
        "import {} {{ functions = {}; }}",
        string(&nix_dir()?.to_string_lossy()),
        string(&functions_dir()?.to_string_lossy()),
    ))
}

/// Evaluates the Nix expression `expr` strictly to JSON; `path` names what
/// is evaluated in the errors.
pub fn eval_expr(path: &Path, expr: &str) -> Result<Value, Error> {
    let output = Command::new(EVALUATOR)
        .args(["--eval", "--strict", "--json", "--expr", expr])
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::NotFound { source })?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // The message itself, not the expression Nix quotes around it.
        if stderr.contains(&format!("error: {NOT_A_FUNCTION}")) {
            return Err(Error::NotAFunction { path: path.into() });
        }
        return Err(Error::Eval {
            path: path.into(),
            detail: stderr.trim().to_owned(),
        });
    }
    serde_json::from_slice(&output.stdout).map_err(|error| Error::Eval {
        path: path.into(),
        detail: error.to_string(),
    })
}

/// Evaluates the file `path`, a function of `{ agl, ... }` (D8), applied to
/// the `agl` argument; a file that is not a function is an error.
pub fn eval_file(path: &Path) -> Result<Value, Error> {
    let expr = format!(
        "let f = import {file}; in if builtins.isFunction f then f {{ agl = {agl}; }} else throw {msg}",
        file = string(&path.to_string_lossy()),
        agl = agl_expr()?,
        msg = string(NOT_A_FUNCTION),
    );
    eval_expr(path, &expr)
}

/// Evaluates `expr` with `agl` bound to the `agl` argument; `path` names it
/// in the errors (a built-in, `<built-in>/…`).
pub fn eval_with_agl(path: &Path, expr: &str) -> Result<Value, Error> {
    eval_expr(path, &format!("let agl = {}; in {expr}", agl_expr()?))
}

/// The keywords of Nix, which an attribute name cannot be unquoted.
const KEYWORDS: &[&str] = &["if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or"];

/// An attribute name: bare when it is an identifier and not a keyword,
/// quoted otherwise.
pub fn key(name: &str) -> String {
    let mut chars = name.chars();
    let identifier = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '\'' | '-'));
    if identifier && !KEYWORDS.contains(&name) {
        name.to_owned()
    } else {
        string(name)
    }
}

/// A double-quoted Nix string: `\`, `"`, `${` and the control characters
/// escaped (AGL-039, D13 as corrected).
pub fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON value as a Nix expression on one line.
pub fn render(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => string(text),
        Value::Array(items) if items.is_empty() => "[ ]".into(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(render).collect();
            format!("[ {} ]", items.join(" "))
        }
        Value::Object(map) if map.is_empty() => "{ }".into(),
        Value::Object(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(name, value)| format!("{} = {};", key(name), render(value)))
                .collect();
            format!("{{ {} }}", entries.join(" "))
        }
    }
}

/// An attribute set with one key per line, as agl writes a whole file.
pub fn render_lines(map: &Map<String, Value>) -> String {
    let mut out = String::from("{\n");
    for (name, value) in map {
        out.push_str(&format!("  {} = {};\n", key(name), render(value)));
    }
    out.push('}');
    out
}

/// Writes the settings file `path` anew from `map` (AGL-039, D11, D12):
/// `{ agl, ... }: { … }`, one key per line; comments, `let` and `import`
/// of the old file are not kept.
pub fn write_settings(path: &Path, map: &Map<String, Value>) -> Result<(), error::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| error::Error::from(agentlibre_common::error::Error::Write {
            path: dir.into(),
            source,
        }))?;
    }
    let text = format!("{{ agl, ... }}: {}\n", render_lines(map));
    Ok(agentlibre_common::file::write_atomic(path, &text)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The tests evaluate with the evaluator of the dev shell or the
    /// sandbox; `NIX_REMOTE=dummy://` keeps the store out (AGL-040).
    fn eval(expr: &str) -> Value {
        eval_expr(Path::new("<test>"), expr).unwrap()
    }

    #[test]
    fn a_rendered_value_evaluates_to_itself() {
        let value = json!({
            "else": 1,
            "a-b": [true, false, null],
            "qwen3.8": { "nested": { "x": 1.5, "y": -2 } },
            "task-spec": { "anchor": "", "last": "f00" },
            "strings": ["plain", "q\"uote", "back\\slash", "${interp}", "$${x}", "a$b", "line\nbreak\ttab", "''", "кирилиця"],
            "empty": { "list": [], "set": {} },
            "1x": "a key that starts with a digit",
        });
        assert_eq!(eval(&render(&value)), value);
        let Value::Object(map) = &value else { unreachable!() };
        assert_eq!(eval(&render_lines(map)), value);
        // A float stays a float: `1.0` is read back as `1.0`.
        assert_eq!(eval(&render(&json!({ "t": 1.0 }))), json!({ "t": 1.0 }));
    }

    #[test]
    fn keys_are_bare_unless_they_must_be_quoted() {
        assert_eq!(key("otherwise"), "otherwise");
        assert_eq!(key("task-spec"), "task-spec");
        assert_eq!(key("else"), "\"else\"");
        assert_eq!(key("qwen3.8"), "\"qwen3.8\"");
        assert_eq!(key(""), "\"\"");
    }

    #[test]
    fn an_evaluation_error_names_the_file() {
        match eval_expr(Path::new("/f.nix"), "{ a = ; }") {
            Err(Error::Eval { path, detail }) => {
                assert_eq!(path, Path::new("/f.nix"));
                assert!(detail.contains("syntax error"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }
}
