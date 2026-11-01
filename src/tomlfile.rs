//! Reading TOML files strictly: unknown keys and wrong types are errors
//! that name the file and the key (D60).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::error::{Error, Expected};

/// Reads and parses `path`; `None` when it does not exist.
pub fn read(path: &Path) -> Result<Option<Table>, Error> {
    match fs::read_to_string(path) {
        Ok(text) => parse(path, &text).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Read {
            path: path.into(),
            source,
        }),
    }
}

/// Parses `text`; `path` names it in errors.
pub fn parse(path: &Path, text: &str) -> Result<Table, Error> {
    text.parse().map_err(|error: toml::de::Error| Error::Parse {
        path: path.into(),
        detail: error.to_string().trim_end().to_owned(),
    })
}

/// A TOML basic string, escaped.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `text` with the top-level `key` set to the string `value` by one line
/// (AGL-036, D7): the line of the key is replaced, or a line is inserted
/// after the last line of the top level that is not blank or a comment
/// (at the start when there is none); the rest of the text is kept byte
/// for byte. The result must parse to the same table but for `key`, or it
/// is an error (a key this edit cannot place, as a multi-line value of
/// the key itself); `path` names the file in errors.
pub fn set_top_string(path: &Path, text: &str, key: &str, value: &str) -> Result<String, Error> {
    let before = parse(path, text)?;
    let line = format!("{key} = {}", quote(value));
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    // The top level ends at the first table header.
    let top = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    let is_key = |l: &str| {
        l.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let mut out = String::with_capacity(text.len() + line.len() + 1);
    match lines[..top].iter().position(|l| is_key(l)) {
        Some(at) => {
            for (i, l) in lines.iter().enumerate() {
                if i == at {
                    out.push_str(&line);
                    out.push_str(if l.ends_with('\n') { "\n" } else { "" });
                } else {
                    out.push_str(l);
                }
            }
        }
        None => {
            let after = lines[..top]
                .iter()
                .rposition(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#')
                })
                .map_or(0, |i| i + 1);
            for (i, l) in lines.iter().enumerate() {
                if i == after {
                    out.push_str(&line);
                    out.push('\n');
                }
                out.push_str(l);
                if i + 1 == after && !l.ends_with('\n') {
                    out.push('\n');
                }
            }
            if after >= lines.len() {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    let mut expected = before;
    expected.insert(key.into(), Value::String(value.into()));
    if parse(path, &out)? != expected {
        return Err(Error::Parse {
            path: path.into(),
            detail: format!("`{key}` cannot be set by one line in this file"),
        });
    }
    Ok(out)
}

/// Rejects a Function or format name that cannot name a file in its
/// directory.
pub fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(Error::InvalidName(name.into()));
    }
    Ok(())
}

/// A table being read. Keys are removed as they are read.
pub struct Section {
    path: PathBuf,
    /// Location of the table in the file, such as `server.`.
    prefix: String,
    table: Table,
}

impl Section {
    pub fn new(path: &Path, table: Table) -> Section {
        Section::at(path, "", table)
    }

    /// A table read at the location `prefix`, such as `task-spec.`;
    /// an empty `prefix` is the top level.
    pub fn at(path: &Path, prefix: &str, table: Table) -> Section {
        Section {
            path: path.into(),
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}.")
            },
            table,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consumes the section into the raw `Value` of its table, for a
    /// table carried unparsed.
    pub fn into_value(self) -> Value {
        Value::Table(self.table)
    }

    /// The full name of `key` in the file; with an empty `key`, the name of
    /// this table.
    pub fn key(&self, key: &str) -> String {
        if key.is_empty() {
            return self.prefix.trim_end_matches('.').to_owned();
        }
        format!("{}{key}", self.prefix)
    }

    /// Rejects a key outside `allowed`.
    pub fn check_keys(&self, allowed: &'static [&'static str]) -> Result<(), Error> {
        match self
            .table
            .keys()
            .find(|key| !allowed.contains(&key.as_str()))
        {
            Some(key) => Err(Error::UnknownKey {
                path: self.path.clone(),
                key: self.key(key),
                allowed,
            }),
            None => Ok(()),
        }
    }

    /// Takes the string `key`, if present.
    pub fn string(&mut self, key: &str) -> Result<Option<String>, Error> {
        match self.table.remove(key) {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value)),
            Some(_) => Err(self.wrong_type(key, Expected::String)),
        }
    }

    /// Takes the string `key`, which must be present.
    pub fn required_string(&mut self, key: &str) -> Result<String, Error> {
        self.string(key)?.ok_or_else(|| self.missing(key))
    }

    /// Takes the integer `key`, which must be present.
    pub fn required_integer(&mut self, key: &str) -> Result<i64, Error> {
        self.integer(key)?.ok_or_else(|| self.missing(key))
    }

    /// Takes the array of strings `key`, which must be present.
    pub fn required_strings(&mut self, key: &str) -> Result<Vec<String>, Error> {
        self.strings(key)?.ok_or_else(|| self.missing(key))
    }

    /// Takes the boolean `key`, if present.
    pub fn bool(&mut self, key: &str) -> Result<Option<bool>, Error> {
        match self.table.remove(key) {
            None => Ok(None),
            Some(Value::Boolean(value)) => Ok(Some(value)),
            Some(_) => Err(self.wrong_type(key, Expected::Bool)),
        }
    }

    /// Takes the integer `key`, if present.
    pub fn integer(&mut self, key: &str) -> Result<Option<i64>, Error> {
        match self.table.remove(key) {
            None => Ok(None),
            Some(Value::Integer(value)) => Ok(Some(value)),
            Some(_) => Err(self.wrong_type(key, Expected::Integer)),
        }
    }

    /// Takes the number `key`, integer or float, if present.
    pub fn number(&mut self, key: &str) -> Result<Option<f64>, Error> {
        match self.table.remove(key) {
            None => Ok(None),
            Some(Value::Float(value)) => Ok(Some(value)),
            Some(Value::Integer(value)) => Ok(Some(value as f64)),
            Some(_) => Err(self.wrong_type(key, Expected::Number)),
        }
    }

    /// Takes the array of strings `key`, if present.
    pub fn strings(&mut self, key: &str) -> Result<Option<Vec<String>>, Error> {
        let wrong = |section: &Section| section.wrong_type(key, Expected::Strings);
        match self.table.remove(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .into_iter()
                .map(|item| match item {
                    Value::String(text) => Ok(text),
                    _ => Err(wrong(self)),
                })
                .collect::<Result<_, _>>()
                .map(Some),
            Some(_) => Err(wrong(self)),
        }
    }

    /// Takes every remaining key as a table, in key order.
    pub fn sections(&mut self) -> Result<Vec<(String, Section)>, Error> {
        let keys: Vec<String> = self.table.keys().cloned().collect();
        keys.into_iter()
            .map(|key| {
                let section = self.required_section(&key)?;
                Ok((key, section))
            })
            .collect()
    }

    /// Takes every remaining key as a string, in key order.
    pub fn string_entries(&mut self) -> Result<Vec<(String, String)>, Error> {
        let keys: Vec<String> = self.table.keys().cloned().collect();
        keys.into_iter()
            .map(|key| {
                let value = self.required_string(&key)?;
                Ok((key, value))
            })
            .collect()
    }

    /// Rejects any key left in the table: the known keys were read, so
    /// a remainder is an unknown key.
    pub fn check_empty(&self) -> Result<(), Error> {
        match self.table.keys().next() {
            Some(key) => Err(Error::ArtifactKey {
                path: self.path.clone(),
                key: self.key(key),
            }),
            None => Ok(()),
        }
    }

    /// Takes the table `key`, if present.
    pub fn section(&mut self, key: &str) -> Result<Option<Section>, Error> {
        match self.table.remove(key) {
            None => Ok(None),
            Some(Value::Table(table)) => Ok(Some(Section {
                path: self.path.clone(),
                prefix: format!("{}{key}.", self.prefix),
                table,
            })),
            Some(_) => Err(self.wrong_type(key, Expected::Table)),
        }
    }

    /// Takes the table `key`, which must be present.
    pub fn required_section(&mut self, key: &str) -> Result<Section, Error> {
        self.section(key)?.ok_or_else(|| self.missing(key))
    }

    fn missing(&self, key: &str) -> Error {
        Error::MissingKey {
            path: self.path.clone(),
            key: self.key(key),
        }
    }

    fn wrong_type(&self, key: &str, expected: Expected) -> Error {
        Error::WrongType {
            path: self.path.clone(),
            key: self.key(key),
            expected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(text: &str, value: &str) -> Result<String, Error> {
        set_top_string(Path::new("/c/agentLIBRE.toml"), text, "function", value)
    }

    #[test]
    fn the_key_line_is_replaced_and_the_rest_kept() {
        // AGL-036, D7: the comments and the tables stay byte for byte.
        let text = "# the profile\nfunction = \"agentLIBRE\" # mine\n\n# servers\n[server.llama]\nurl = \"http://x\"\nfunction = \"not top\"\n";
        assert_eq!(
            set(text, "agentLIBRE-strata").unwrap(),
            "# the profile\nfunction = \"agentLIBRE-strata\"\n\n# servers\n[server.llama]\nurl = \"http://x\"\nfunction = \"not top\"\n"
        );
        // A key that only starts with the name is another key.
        let text = "functions = 1\nfunction = \"a\"\n";
        assert_eq!(set(text, "b").unwrap(), "functions = 1\nfunction = \"b\"\n");
    }

    #[test]
    fn a_missing_key_goes_after_the_last_top_level_line() {
        let text = "format = 1\nworkspace_sha256 = [\"sha256:x\"]\n\n# a comment of the table\n[server.llama]\nurl = \"http://x\"\n";
        assert_eq!(
            set(text, "m").unwrap(),
            "format = 1\nworkspace_sha256 = [\"sha256:x\"]\nfunction = \"m\"\n\n# a comment of the table\n[server.llama]\nurl = \"http://x\"\n"
        );
        // Only comments and tables: at the start.
        assert_eq!(
            set("# c\n[search]\nurl = \"https://x\"\n", "m").unwrap(),
            "function = \"m\"\n# c\n[search]\nurl = \"https://x\"\n"
        );
        // An empty file, and a last line without its newline.
        assert_eq!(set("", "m").unwrap(), "function = \"m\"\n");
        assert_eq!(set("format = 1", "m").unwrap(), "format = 1\nfunction = \"m\"\n");
        // The value is quoted.
        assert_eq!(set("", "a\"b").unwrap(), "function = \"a\\\"b\"\n");
    }

    #[test]
    fn an_edit_that_would_change_more_is_refused() {
        // A multi-line value of the key cannot be replaced by one line.
        let text = "function = \"\"\"\nx\n\"\"\"\n";
        assert!(matches!(set(text, "m"), Err(Error::Parse { .. })));
        // A file that does not parse is an error before any edit.
        assert!(matches!(set("function =", "m"), Err(Error::Parse { .. })));
    }
}
