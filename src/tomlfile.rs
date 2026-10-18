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

    pub fn contains(&self, key: &str) -> bool {
        self.table.contains_key(key)
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
