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

/// A table being read. Keys are removed as they are read.
pub struct Section {
    path: PathBuf,
    /// Location of the table in the file, such as `server.`.
    prefix: String,
    table: Table,
}

impl Section {
    pub fn new(path: &Path, table: Table) -> Section {
        Section {
            path: path.into(),
            prefix: String::new(),
            table,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The full name of `key` in the file.
    pub fn key(&self, key: &str) -> String {
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
