//! Reading an evaluated Nix value strictly (AGL-039): unknown keys,
//! missing keys and wrong types are errors that name the file and the key
//! path, as `tomlfile::Section` does for TOML (D60).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::{Error, Expected};

/// An attribute set being read. Keys are removed as they are read.
pub struct Section {
    path: PathBuf,
    /// Location of the set in the file, such as `policy.state.work.`.
    prefix: String,
    map: Map<String, Value>,
}

impl Section {
    /// The top level of the evaluated file `path`, which must be a set.
    pub fn top(path: &Path, value: Value) -> Result<Section, Error> {
        match value {
            Value::Object(map) => Ok(Section {
                path: path.into(),
                prefix: String::new(),
                map,
            }),
            _ => Err(Error::WrongType {
                path: path.into(),
                key: String::new(),
                expected: Expected::Table,
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The full name of `key` in the file; with an empty `key`, the name of
    /// this set.
    pub fn key(&self, key: &str) -> String {
        if key.is_empty() {
            return self.prefix.trim_end_matches('.').to_owned();
        }
        format!("{}{key}", self.prefix)
    }

    /// Rejects a key outside `allowed`.
    pub fn check_keys(&self, allowed: &'static [&'static str]) -> Result<(), Error> {
        match self.map.keys().find(|key| !allowed.contains(&key.as_str())) {
            Some(key) => Err(Error::UnknownKey {
                path: self.path.clone(),
                key: self.key(key),
                allowed,
            }),
            None => Ok(()),
        }
    }

    /// Whether `key` is still in the set.
    pub fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// The keys still in the set, in key order.
    pub fn keys(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    /// Takes the string `key`, if present.
    pub fn string(&mut self, key: &str) -> Result<Option<String>, Error> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value)),
            Some(_) => Err(self.wrong_type(key, Expected::String)),
        }
    }

    /// Takes the string `key`, which must be present.
    pub fn required_string(&mut self, key: &str) -> Result<String, Error> {
        self.string(key)?.ok_or_else(|| self.missing(key))
    }

    /// Takes the boolean `key`, if present.
    pub fn bool(&mut self, key: &str) -> Result<Option<bool>, Error> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(value)),
            Some(_) => Err(self.wrong_type(key, Expected::Bool)),
        }
    }

    /// Takes the integer `key`, if present.
    pub fn integer(&mut self, key: &str) -> Result<Option<i64>, Error> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Number(number)) if number.is_i64() => Ok(number.as_i64()),
            Some(_) => Err(self.wrong_type(key, Expected::Integer)),
        }
    }

    /// Takes the integer `key`, which must be present.
    pub fn required_integer(&mut self, key: &str) -> Result<i64, Error> {
        self.integer(key)?.ok_or_else(|| self.missing(key))
    }

    /// Takes the number `key`, integer or float, if present.
    pub fn number(&mut self, key: &str) -> Result<Option<f64>, Error> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Number(number)) => Ok(number.as_f64()),
            Some(_) => Err(self.wrong_type(key, Expected::Number)),
        }
    }

    /// Takes the list of strings `key`, if present.
    pub fn strings(&mut self, key: &str) -> Result<Option<Vec<String>>, Error> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .into_iter()
                .map(|item| match item {
                    Value::String(text) => Ok(text),
                    _ => Err(self.wrong_type(key, Expected::Strings)),
                })
                .collect::<Result<_, _>>()
                .map(Some),
            Some(_) => Err(self.wrong_type(key, Expected::Strings)),
        }
    }

    /// Takes the list of strings `key`, which must be present.
    pub fn required_strings(&mut self, key: &str) -> Result<Vec<String>, Error> {
        self.strings(key)?.ok_or_else(|| self.missing(key))
    }

    /// Takes the set `key`, if present.
    pub fn section(&mut self, key: &str) -> Result<Option<Section>, Error> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Object(map)) => Ok(Some(Section {
                path: self.path.clone(),
                prefix: format!("{}{key}.", self.prefix),
                map,
            })),
            Some(_) => Err(self.wrong_type(key, Expected::Table)),
        }
    }

    /// Takes the set `key`, which must be present.
    pub fn required_section(&mut self, key: &str) -> Result<Section, Error> {
        self.section(key)?.ok_or_else(|| self.missing(key))
    }

    /// Takes every remaining key as a set, in key order.
    pub fn sections(&mut self) -> Result<Vec<(String, Section)>, Error> {
        self.keys()
            .into_iter()
            .map(|key| {
                let section = self.required_section(&key)?;
                Ok((key, section))
            })
            .collect()
    }

    /// Takes every remaining key as a string, in key order.
    pub fn string_entries(&mut self) -> Result<Vec<(String, String)>, Error> {
        self.keys()
            .into_iter()
            .map(|key| {
                let value = self.required_string(&key)?;
                Ok((key, value))
            })
            .collect()
    }

    /// The error of a value of `key` that is not what it must be.
    pub fn wrong_type(&self, key: &str, expected: Expected) -> Error {
        Error::WrongType {
            path: self.path.clone(),
            key: self.key(key),
            expected,
        }
    }

    /// The error of a missing `key`.
    pub fn missing(&self, key: &str) -> Error {
        Error::MissingKey {
            path: self.path.clone(),
            key: self.key(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn top(value: Value) -> Section {
        Section::top(Path::new("/f.nix"), value).unwrap()
    }

    #[test]
    fn keys_are_read_strictly_with_their_path() {
        let mut file = top(json!({ "a": { "b": { "c": 1 } }, "s": "x", "n": 2, "f": 0.5, "l": ["a"] }));
        file.check_keys(&["a", "s", "n", "f", "l"]).unwrap();
        assert!(matches!(file.check_keys(&["a"]), Err(Error::UnknownKey { key, .. }) if key == "f" || key == "l" || key == "n" || key == "s"));
        assert_eq!(file.string("s").unwrap().as_deref(), Some("x"));
        assert_eq!(file.integer("n").unwrap(), Some(2));
        // An integer is also a number; a float is not an integer.
        assert_eq!(file.number("f").unwrap(), Some(0.5));
        assert_eq!(file.strings("l").unwrap(), Some(vec!["a".to_owned()]));
        let mut a = file.required_section("a").unwrap();
        let mut b = a.required_section("b").unwrap();
        assert!(matches!(b.string("c"), Err(Error::WrongType { key, .. }) if key == "a.b.c"));
        assert!(matches!(b.required_string("d"), Err(Error::MissingKey { key, .. }) if key == "a.b.d"));
        let mut file = top(json!({ "f": 1.5 }));
        assert!(matches!(file.integer("f"), Err(Error::WrongType { .. })));
        assert!(Section::top(Path::new("/f.nix"), json!([1])).is_err());
    }
}
