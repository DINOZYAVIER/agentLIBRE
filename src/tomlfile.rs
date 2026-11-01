//! Reading TOML strictly: unknown keys and wrong types are errors that
//! name the file and the key (D60). Only the forge's front matter and the
//! project marker are TOML until AGL-041.

use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::error::{Error, Expected};

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

    /// Takes the integer `key`, if present.
    pub fn integer(&mut self, key: &str) -> Result<Option<i64>, Error> {
        match self.table.remove(key) {
            None => Ok(None),
            Some(Value::Integer(value)) => Ok(Some(value)),
            Some(_) => Err(self.wrong_type(key, Expected::Integer)),
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
