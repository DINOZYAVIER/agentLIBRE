//! Nix as the language of the Functions and the settings (AGL-039, D6-D13,
//! D38, D39): the evaluation of a file by the system's `nix-instantiate`,
//! the JSON it gives, and the writing of a value back as Nix.

pub mod section;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::error::Error;

pub use section::Section;

/// The variable of agl's `nix/` directory (AGL-039, D7, D34).
pub const NIX_VAR: &str = "AGENTLIBRE_NIX";

/// The evaluator, the system's, from `PATH` (AGL-039, D38, D39).
const EVALUATOR: &str = "nix-instantiate";

/// What the evaluation throws when a file is not a function (D8); agl
/// recognizes it in the evaluator's message.
const NOT_A_FUNCTION: &str = "agentLIBRE: the file is not a function of { agl, ... }";

/// agl's `nix/` directory: the built-in Functions, formats, policies and
/// models, the `agl` argument of every file (AGL-039, D7, D35).
pub fn nix_dir() -> Result<PathBuf, Error> {
    std::env::var_os(NIX_VAR)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or(Error::NixDirMissing)
}

/// The `agl` argument, as a Nix expression.
fn agl_expr() -> Result<String, Error> {
    Ok(format!("import {}", string(&nix_dir()?.to_string_lossy())))
}

/// Evaluates the Nix expression `expr` strictly to JSON; `path` names what
/// is evaluated in the errors.
pub fn eval_expr(path: &Path, expr: &str) -> Result<Value, Error> {
    let output = Command::new(EVALUATOR)
        .args(["--eval", "--strict", "--json", "--expr", expr])
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::NixNotFound { source })?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // The message itself, not the expression Nix quotes around it.
        if stderr.contains(&format!("error: {NOT_A_FUNCTION}")) {
            return Err(Error::NixNotAFunction { path: path.into() });
        }
        return Err(Error::NixEval {
            path: path.into(),
            detail: stderr.trim().to_owned(),
        });
    }
    serde_json::from_slice(&output.stdout).map_err(|error| Error::NixEval {
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
pub fn write_settings(path: &Path, map: &Map<String, Value>) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| Error::Write {
            path: dir.into(),
            source,
        })?;
    }
    let text = format!("{{ agl, ... }}: {}\n", render_lines(map));
    crate::forge::write_atomic(path, &text)
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
            Err(Error::NixEval { path, detail }) => {
                assert_eq!(path, Path::new("/f.nix"));
                assert!(detail.contains("syntax error"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }
}
