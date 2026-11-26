//! A kind of artifact is a description in a Nix file (AGL-124, D6): its
//! statuses and their edges, its fields and the kind of each, its body,
//! which actions the forge tool allows in which status and how a new one
//! is named. The forge holds the mechanism; a kind is data.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::Error;
use crate::forge::error::{Error as ForgeError, KindFault};
use crate::forge::{Artifact, ENVELOPE};
use agentlibre_common::nix::{self, Expected, Section};

/// The directory of the descriptions in a store, the user's config and
/// agl's `nix/` (AGL-124, D11).
pub const DIR: &str = "forge-types";

/// The kind of a field: one of the twelve the mechanism knows (AGL-124,
/// D8); a description chooses among them and adds none of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// A line.
    String,
    /// Long text.
    Text,
    Number,
    Bool,
    /// `YYYY-MM-DD`, as `forge::check_date`.
    Date,
    /// One of the set the description lists (`choices`, D23).
    Choice(Vec<String>),
    /// Values of one kind (`of`, D23).
    List(Box<FieldKind>),
    /// An artifact name that must exist; `rename` rewrites it.
    Ref,
    /// A list of artifact names.
    Refs,
    /// A commit, resolved by `git rev-parse`.
    Commit,
    /// Two commits, `{ anchor; last; }`, the first an ancestor of the
    /// second.
    Range,
    /// The commands `forge check` runs.
    Cmds,
}

impl FieldKind {
    /// The name a description writes.
    pub fn name(&self) -> String {
        match self {
            FieldKind::String => "string".into(),
            FieldKind::Text => "text".into(),
            FieldKind::Number => "number".into(),
            FieldKind::Bool => "bool".into(),
            FieldKind::Date => "date".into(),
            FieldKind::Choice(_) => "choice".into(),
            FieldKind::List(of) => format!("list of {}", of.name()),
            FieldKind::Ref => "ref".into(),
            FieldKind::Refs => "refs".into(),
            FieldKind::Commit => "commit".into(),
            FieldKind::Range => "range".into(),
            FieldKind::Cmds => "cmds".into(),
        }
    }

    /// The value a field of this kind starts with when its description
    /// sets no `default`: empty, `today` for a date, the first choice.
    fn empty(&self, today: &str) -> Value {
        match self {
            FieldKind::String | FieldKind::Text | FieldKind::Ref | FieldKind::Commit => Value::String(String::new()),
            FieldKind::Number => Value::from(0),
            FieldKind::Bool => Value::Bool(false),
            FieldKind::Date => Value::String(today.into()),
            FieldKind::Choice(choices) => Value::String(choices.first().cloned().unwrap_or_default()),
            FieldKind::List(_) | FieldKind::Refs | FieldKind::Cmds => Value::Array(Vec::new()),
            FieldKind::Range => serde_json::json!({ "anchor": "", "last": "" }),
        }
    }

    /// Checks `value` of the field `key` of the file `path`.
    pub fn check(&self, path: &Path, key: &str, value: &Value) -> Result<(), Error> {
        let wrong = |expected| -> Error {
            nix::Error::WrongType {
                path: path.into(),
                key: key.into(),
                expected,
            }
            .into()
        };
        match self {
            FieldKind::String | FieldKind::Text | FieldKind::Ref | FieldKind::Commit => {
                value.as_str().map(|_| ()).ok_or_else(|| wrong(Expected::String))
            }
            FieldKind::Number => value.as_f64().map(|_| ()).ok_or_else(|| wrong(Expected::Number)),
            FieldKind::Bool => value.as_bool().map(|_| ()).ok_or_else(|| wrong(Expected::Bool)),
            FieldKind::Date => {
                let text = value.as_str().ok_or_else(|| wrong(Expected::String))?;
                crate::forge::check_date(key, text, path)
            }
            FieldKind::Choice(choices) => {
                let text = value.as_str().ok_or_else(|| wrong(Expected::String))?;
                if choices.iter().any(|choice| choice == text) {
                    Ok(())
                } else {
                    Err(ForgeError::FieldChoice {
                        path: path.into(),
                        key: key.into(),
                        value: text.into(),
                        choices: choices.clone(),
                    }
                    .into())
                }
            }
            FieldKind::Refs | FieldKind::Cmds => match value {
                Value::Array(items) if items.iter().all(Value::is_string) => Ok(()),
                _ => Err(wrong(Expected::Strings)),
            },
            FieldKind::List(of) => {
                let Value::Array(items) = value else {
                    return Err(wrong(Expected::Strings));
                };
                for (i, item) in items.iter().enumerate() {
                    of.check(path, &format!("{key}[{i}]"), item)?;
                }
                Ok(())
            }
            FieldKind::Range => {
                // Strictly `anchor` and `last`, both strings (AGL-044).
                let mut section = Section::at(path, key, value.clone())?;
                section.check_keys(&["anchor", "last"])?;
                section.required_string("anchor")?;
                section.required_string("last")?;
                Ok(())
            }
        }
    }
}

/// A field a description declares (AGL-124, D9).
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub kind: FieldKind,
    /// Whether the key must be in the file.
    pub required: bool,
    /// The value the scaffold writes; the kind's empty value when unset.
    pub default: Option<Value>,
    /// What the field is, shown to the model and in `forge show`.
    pub description: String,
}

/// A section of the body (AGL-124, D13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodySection {
    pub heading: String,
    /// The one-line hint the scaffold writes as an HTML comment.
    pub hint: String,
    /// The section only grows, through `append`.
    pub append_only: bool,
}

/// The statuses in which the forge tool allows each action (AGL-124,
/// D13); `rename` is allowed in any status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Actions {
    pub edit: Vec<String>,
    pub append: Vec<String>,
    pub remove: Vec<String>,
}

/// A kind of artifact, as its description sets it (AGL-124).
#[derive(Clone, Debug, PartialEq)]
pub struct Kind {
    /// The value of `type` in the files.
    pub name: String,
    /// The description file.
    pub path: PathBuf,
    pub statuses: Vec<String>,
    /// The status a new artifact starts in (D23).
    pub initial: String,
    /// The statuses each one may move to (D23); one with none is terminal.
    pub edges: BTreeMap<String, Vec<String>>,
    /// By name; a dotted name is a nested key (D10).
    pub fields: BTreeMap<String, Field>,
    pub body: Vec<BodySection>,
    pub actions: Actions,
    /// The prefix of new names (D15).
    pub prefix: Option<String>,
}

/// What a description's file evaluates to when it is not a set.
const NOT_A_SET: &str = "agentLIBRE: the description is not an attribute set";

impl Kind {
    /// Reads and checks the description `path` of the kind `name`
    /// (AGL-124, D18).
    pub fn load(name: &str, path: &Path) -> Result<Kind, Error> {
        let value = nix::eval_expr(path, &expr(path)).map_err(|error| match error {
            nix::Error::Eval { detail, .. } if detail.contains(&format!("error: {NOT_A_SET}")) => {
                fault(path, KindFault::NotASet)
            }
            error => error.into(),
        })?;
        Kind::of_value(name, path, value)
    }

    /// The kind `name` of the evaluated description `value` of `path`,
    /// checked whole (AGL-124, D18).
    pub fn of_value(name: &str, path: &Path, value: Value) -> Result<Kind, Error> {
        let mut top = Section::top(path, value)?;
        top.check_keys(&["statuses", "initial", "edges", "fields", "body", "actions", "prefix"])?;
        let statuses = top.required_strings("statuses")?;
        if statuses.is_empty() {
            return Err(fault(path, KindFault::NoStatuses));
        }
        for (i, status) in statuses.iter().enumerate() {
            if statuses[..i].contains(status) {
                return Err(fault(path, KindFault::DuplicateStatus { status: status.clone() }));
            }
        }
        let known = |key: String, status: &str| -> Result<(), Error> {
            if statuses.iter().any(|known| known == status) {
                Ok(())
            } else {
                Err(fault(path, KindFault::UnknownStatus { key, status: status.into() }))
            }
        };
        let initial = top.required_string("initial")?;
        known("initial".into(), &initial)?;
        let mut edges = BTreeMap::new();
        if let Some(mut set) = top.section("edges")? {
            for from in set.keys() {
                known(format!("edges.{from}"), &from)?;
                let to = set.required_strings(&from)?;
                for status in &to {
                    known(format!("edges.{from}"), status)?;
                }
                edges.insert(from, to);
            }
        }
        let mut fields = BTreeMap::new();
        if let Some(mut set) = top.section("fields")? {
            for (field_name, mut declared) in set.sections()? {
                fields.insert(field_name.clone(), field(path, &field_name, &mut declared)?);
            }
        }
        check_field_names(path, &fields)?;
        let mut ranges = fields.iter().filter(|(_, field)| field.kind == FieldKind::Range).map(|(name, _)| name);
        if let (Some(first), Some(second)) = (ranges.next(), ranges.next()) {
            return Err(fault(path, KindFault::TwoRanges { first: first.clone(), second: second.clone() }));
        }
        let mut body = Vec::new();
        for mut section in top.tables("body")?.unwrap_or_default() {
            section.check_keys(&["heading", "hint", "appendOnly"])?;
            let heading = section.required_string("heading")?;
            if heading.trim().is_empty() || heading.contains('\n') {
                return Err(fault(path, KindFault::Heading { heading }));
            }
            if body.iter().any(|known: &BodySection| known.heading == heading) {
                return Err(fault(path, KindFault::DuplicateHeading { heading }));
            }
            body.push(BodySection {
                heading,
                hint: section.string("hint")?.unwrap_or_default(),
                append_only: section.bool("appendOnly")?.unwrap_or(false),
            });
        }
        let mut actions = Actions::default();
        if let Some(mut set) = top.section("actions")? {
            set.check_keys(&["edit", "append", "remove"])?;
            for (action, list) in [("edit", &mut actions.edit), ("append", &mut actions.append), ("remove", &mut actions.remove)] {
                *list = set.strings(action)?.unwrap_or_default();
                for status in list.iter() {
                    known(format!("actions.{action}"), status)?;
                }
            }
        }
        let prefix = top.string("prefix")?;
        if let Some(prefix) = &prefix
            && (prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_alphabetic()))
        {
            return Err(fault(path, KindFault::Prefix { prefix: prefix.clone() }));
        }
        Ok(Kind {
            name: name.into(),
            path: path.into(),
            statuses,
            initial,
            edges,
            fields,
            body,
            actions,
            prefix,
        })
    }

    /// Whether the status `from` may move to `to`.
    pub fn allows(&self, from: &str, to: &str) -> bool {
        self.edges.get(from).is_some_and(|targets| targets.iter().any(|target| target == to))
    }

    /// Whether nothing moves from `status`.
    pub fn terminal(&self, status: &str) -> bool {
        self.edges.get(status).is_none_or(Vec::is_empty)
    }

    /// The statuses in which the forge tool allows `action`.
    pub fn allowed(&self, action: &str) -> &[String] {
        match action {
            "edit" => &self.actions.edit,
            "append" => &self.actions.append,
            "remove" => &self.actions.remove,
            _ => &[],
        }
    }

    /// The headings of the append-only sections.
    pub fn append_only(&self) -> impl Iterator<Item = &str> {
        self.body.iter().filter(|section| section.append_only).map(|section| section.heading.as_str())
    }

    /// The one field of kind `range` (AGL-124, D16).
    pub fn range_field(&self) -> Option<&str> {
        self.fields.iter().find(|(_, field)| field.kind == FieldKind::Range).map(|(name, _)| name.as_str())
    }

    /// The fields that name artifacts: `ref` and `refs`.
    pub fn ref_fields(&self) -> impl Iterator<Item = (&str, &Field)> {
        self.fields
            .iter()
            .filter(|(_, field)| matches!(field.kind, FieldKind::Ref | FieldKind::Refs))
            .map(|(name, field)| (name.as_str(), field))
    }

    /// The fields of commands, which `forge check` runs.
    pub fn cmds_fields(&self) -> impl Iterator<Item = &str> {
        self.fields
            .iter()
            .filter(|(_, field)| field.kind == FieldKind::Cmds)
            .map(|(name, _)| name.as_str())
    }

    /// The scaffold of a new artifact named `name`, dated `today`: the
    /// initial status, every field at its default, the body's sections
    /// each with its hint. `path` is set by the caller.
    pub fn scaffold(&self, name: &str, today: &str) -> Artifact {
        let fields = self
            .fields
            .iter()
            .map(|(field_name, field)| {
                (field_name.clone(), field.default.clone().unwrap_or_else(|| field.kind.empty(today)))
            })
            .collect();
        let sections: Vec<String> = self
            .body
            .iter()
            .map(|section| {
                if section.hint.is_empty() {
                    format!("## {}\n", section.heading)
                } else {
                    format!("## {}\n<!-- {} -->\n", section.heading, section.hint)
                }
            })
            .collect();
        Artifact {
            path: PathBuf::new(),
            type_name: self.name.clone(),
            name: name.into(),
            status: self.initial.clone(),
            created: today.into(),
            updated: today.into(),
            fields,
            body: format!("\n{}", sections.join("\n")),
        }
    }

    /// Takes the fields of a front matter, `rest` being what is left of
    /// it past the envelope: each key a declared field or a set holding
    /// declared fields (D10), each value of its field's kind, every
    /// required field there (D3, D9).
    pub fn take_fields(&self, path: &Path, rest: Map<String, Value>) -> Result<BTreeMap<String, Value>, Error> {
        let mut out = BTreeMap::new();
        self.take_at(path, "", rest, &mut out)?;
        for (field_name, field) in &self.fields {
            if field.required && !out.contains_key(field_name) {
                return Err(nix::Error::MissingKey { path: path.into(), key: field_name.clone() }.into());
            }
        }
        Ok(out)
    }

    fn take_at(&self, path: &Path, prefix: &str, map: Map<String, Value>, out: &mut BTreeMap<String, Value>) -> Result<(), Error> {
        for (key, value) in map {
            let full = if prefix.is_empty() { key } else { format!("{prefix}.{key}") };
            if let Some(field) = self.fields.get(&full) {
                field.kind.check(path, &full, &value)?;
                out.insert(full, value);
            } else if self.fields.keys().any(|name| name.starts_with(&format!("{full}."))) {
                match value {
                    Value::Object(map) => self.take_at(path, &full, map, out)?,
                    _ => {
                        return Err(nix::Error::WrongType { path: path.into(), key: full, expected: Expected::Table }.into());
                    }
                }
            } else {
                return Err(ForgeError::ArtifactKey { path: path.into(), key: full }.into());
            }
        }
        Ok(())
    }
}

/// The error of the description `path` for `fault`.
fn fault(path: &Path, fault: KindFault) -> Error {
    ForgeError::KindDescription { path: path.into(), fault }.into()
}

/// The expression agl evaluates for the description `path`: a plain set
/// (AGL-124, D22), or the throw of [`NOT_A_SET`].
fn expr(path: &Path) -> String {
    format!(
        "(let v = import {}; in if builtins.isAttrs v then v else throw {})",
        nix::string(&path.to_string_lossy()),
        nix::string(NOT_A_SET)
    )
}

/// The field `name` as `declared` (D9, D23).
fn field(path: &Path, name: &str, declared: &mut Section) -> Result<Field, Error> {
    declared.check_keys(&["kind", "required", "default", "description", "choices", "of"])?;
    let kind_name = declared.required_string("kind")?;
    let choices = declared.strings("choices")?;
    let of = declared.string("of")?;
    let kind = field_kind(path, name, &kind_name, choices.clone(), of.as_deref())?;
    // `choices` and `of` belong to the kinds that take them.
    let takes_choices = matches!(&kind, FieldKind::Choice(_))
        || matches!(&kind, FieldKind::List(of) if matches!(**of, FieldKind::Choice(_)));
    if choices.is_some() && !takes_choices {
        return Err(fault(path, KindFault::Stray { field: name.into(), key: "choices".into() }));
    }
    if of.is_some() && !matches!(kind, FieldKind::List(_)) {
        return Err(fault(path, KindFault::Stray { field: name.into(), key: "of".into() }));
    }
    let required = declared.bool("required")?.unwrap_or(false);
    let description = declared.required_string("description")?;
    let default = declared.raw("default");
    if let Some(default) = &default {
        kind.check(path, &format!("fields.{}.default", nix::key(name)), default)
            .map_err(|_| fault(path, KindFault::Default { field: name.into(), kind: kind.name() }))?;
    }
    Ok(Field { kind, required, default, description })
}

/// The kind `kind_name` of the field `name`.
fn field_kind(path: &Path, name: &str, kind_name: &str, choices: Option<Vec<String>>, of: Option<&str>) -> Result<FieldKind, Error> {
    let choice = |choices: Option<Vec<String>>| match choices {
        Some(choices) if !choices.is_empty() => Ok(FieldKind::Choice(choices)),
        _ => Err(fault(path, KindFault::NoChoices { field: name.into() })),
    };
    Ok(match kind_name {
        "string" => FieldKind::String,
        "text" => FieldKind::Text,
        "number" => FieldKind::Number,
        "bool" => FieldKind::Bool,
        "date" => FieldKind::Date,
        "choice" => choice(choices)?,
        "ref" => FieldKind::Ref,
        "refs" => FieldKind::Refs,
        "commit" => FieldKind::Commit,
        "range" => FieldKind::Range,
        "cmds" => FieldKind::Cmds,
        "list" => {
            // A list holds values of one plain kind.
            let item = match of {
                Some("string") => FieldKind::String,
                Some("text") => FieldKind::Text,
                Some("number") => FieldKind::Number,
                Some("bool") => FieldKind::Bool,
                Some("date") => FieldKind::Date,
                Some("choice") => choice(choices)?,
                Some("ref") => FieldKind::Ref,
                Some("commit") => FieldKind::Commit,
                other => {
                    return Err(fault(path, KindFault::ListOf { field: name.into(), of: other.unwrap_or_default().into() }));
                }
            };
            FieldKind::List(Box::new(item))
        }
        other => return Err(fault(path, KindFault::FieldKind { field: name.into(), kind: other.into() })),
    })
}

/// The names of the fields: no empty part, none of the envelope's, none
/// the set of another (D7, D10).
fn check_field_names(path: &Path, fields: &BTreeMap<String, Field>) -> Result<(), Error> {
    for name in fields.keys() {
        let first = name.split('.').next().unwrap_or_default();
        if name.split('.').any(str::is_empty) || ENVELOPE.contains(&first) {
            return Err(fault(path, KindFault::FieldName { field: name.clone() }));
        }
        if let Some(other) = fields.keys().find(|other| other.starts_with(&format!("{name}."))) {
            return Err(fault(path, KindFault::FieldNested { field: name.clone(), inner: other.clone() }));
        }
    }
    Ok(())
}

/// The descriptions a store's commands read (AGL-124, D11, D20, D24): for
/// each type, the first `forge-types/<type>.nix` of the store, the user's
/// config, then agl's; the first found wins whole.
#[derive(Debug, Default)]
pub struct Kinds {
    /// The description file of each type found.
    files: BTreeMap<String, PathBuf>,
    /// The ones that read and checked; a broken one is read again for
    /// its error when it is asked for (D18).
    read: BTreeMap<String, Kind>,
}

impl Kinds {
    /// The kinds of the store `store` (D20).
    pub fn of(store: &Path) -> Kinds {
        let dirs: Vec<PathBuf> = std::iter::once(store.join(DIR)).chain(shared_dirs()).collect();
        Kinds::in_dirs(&dirs)
    }

    /// The kinds of no store: the user's and agl's, for a workspace whose
    /// store is not written yet.
    pub fn shared() -> Kinds {
        Kinds::in_dirs(&shared_dirs())
    }

    /// The kinds of `dirs`, in order: the first found of a type wins.
    pub fn in_dirs(dirs: &[PathBuf]) -> Kinds {
        let mut files = BTreeMap::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "nix"))
                .collect();
            found.sort();
            for path in found {
                if let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()) {
                    files.entry(stem).or_insert(path);
                }
            }
        }
        let read = read_all(&files);
        Kinds { files, read }
    }

    /// The kind of the type `name`: an error when no description of it is
    /// found, or when its description is broken (D18).
    pub fn get(&self, name: &str) -> Result<&Kind, Error> {
        if let Some(kind) = self.read.get(name) {
            return Ok(kind);
        }
        match self.files.get(name) {
            None => Err(ForgeError::UnknownKind { type_name: name.into() }.into()),
            // Read again, for the error it gives.
            Some(path) => Err(Kind::load(name, path).err().unwrap_or_else(|| fault(path, KindFault::NotASet))),
        }
    }

    /// Whether a description of `name` is found, read or broken.
    pub fn knows(&self, name: &str) -> bool {
        self.files.contains_key(name)
    }

    /// The kinds that read and checked, by name.
    pub fn all(&self) -> impl Iterator<Item = &Kind> {
        self.read.values()
    }
}

/// The user's descriptions, then agl's (D11).
fn shared_dirs() -> Vec<PathBuf> {
    let config = agentlibre_common::paths::Paths::from_env(|var| std::env::var_os(var))
        .ok()
        .map(|paths| paths.config.join(DIR));
    let shipped = nix::nix_dir().ok().map(|dir| dir.join(DIR));
    [config, shipped].into_iter().flatten().collect()
}

/// The descriptions of `files`, in one evaluation of all of them; when
/// that fails, one each, so a broken one leaves the others read.
fn read_all(files: &BTreeMap<String, PathBuf>) -> BTreeMap<String, Kind> {
    let mut read = BTreeMap::new();
    if files.is_empty() {
        return read;
    }
    let expr = format!(
        "{{ {} }}",
        files
            .iter()
            .map(|(name, path)| format!("{} = {};", nix::string(name), self::expr(path)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if let Ok(Value::Object(mut values)) = nix::eval_expr(Path::new("<forge-types>"), &expr) {
        for (name, path) in files {
            if let Some(value) = values.remove(name)
                && let Ok(kind) = Kind::of_value(name, path, value)
            {
                read.insert(name.clone(), kind);
            }
        }
        return read;
    }
    for (name, path) in files {
        if let Ok(kind) = Kind::load(name, path) {
            read.insert(name.clone(), kind);
        }
    }
    read
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::tests::shipped;
    use agentlibre_common::testutil::TempDir;
    use std::fs;

    /// A description `text` of `name` in `dir`, read and checked.
    fn described(dir: &Path, name: &str, text: &str) -> Result<Kind, Error> {
        let path = dir.join(format!("{name}.nix"));
        fs::write(&path, text).unwrap();
        Kind::load(name, &path)
    }

    /// The fault of a description `text`, or the error that is not one.
    fn fault_of(text: &str) -> KindFault {
        let dir = TempDir::new();
        match described(dir.path(), "x", text) {
            Err(Error::Forge(ForgeError::KindDescription { fault, .. })) => fault,
            other => panic!("expected a fault, got {other:?}"),
        }
    }

    const MINIMAL: &str = r#"{ statuses = [ "a" "b" ]; initial = "a"; edges = { a = [ "b" ]; }; }"#;

    #[test]
    fn the_shipped_task_spec_carries_today_s_lifecycle() {
        // AGL-124, D13: the edges of AGL-008, the actions of AGL-032.
        let kinds = shipped();
        let spec = kinds.get("task-spec").unwrap();
        let all = ["open", "in-progress", "done", "backlog", "canceled"];
        assert_eq!(spec.statuses, all);
        assert_eq!(spec.initial, "open");
        let legal = [
            ("open", "in-progress"),
            ("open", "backlog"),
            ("open", "canceled"),
            ("in-progress", "done"),
            ("in-progress", "backlog"),
            ("in-progress", "canceled"),
            ("backlog", "in-progress"),
        ];
        for from in all {
            for to in all {
                assert_eq!(spec.allows(from, to), legal.contains(&(from, to)), "{from} -> {to}");
            }
        }
        assert!(spec.terminal("done") && spec.terminal("canceled") && !spec.terminal("backlog"));
        assert_eq!(spec.allowed("edit"), ["open", "in-progress", "backlog"]);
        assert_eq!(spec.allowed("append"), ["open", "in-progress"]);
        assert_eq!(spec.allowed("remove"), ["open", "canceled", "backlog"]);
        assert_eq!(spec.append_only().collect::<Vec<_>>(), ["Progress"]);
        assert_eq!(spec.range_field(), Some("task-spec"));
        assert_eq!(spec.prefix, None);
    }

    #[test]
    fn the_task_spec_scaffold_is_today_s_byte_for_byte() {
        let artifact = shipped().get("task-spec").unwrap().scaffold("agl-008-x", "2026-10-17");
        assert_eq!(artifact.status, "open");
        assert_eq!(artifact.created, "2026-10-17");
        assert_eq!(
            crate::forge::render(&artifact),
            "---\n{\n  schema = \"forge/v2\";\n  type = \"task-spec\";\n  name = \"agl-008-x\";\n  \
             status = \"open\";\n  created = \"2026-10-17\";\n  updated = \"2026-10-17\";\n  deps = [ ];\n  \
             supersedes = [ ];\n  task-spec = { anchor = \"\"; last = \"\"; };\n  verification.cmds = [ ];\n}\n\
             ---\n\n## Objective\n<!-- What this task changes, and its scope. -->\n\n## Decisions\n\
             <!-- Settled choices: the decision, its date, and who set it. -->\n\n## Open \
             questions\n<!-- Questions whose answers change code, stored data or behavior; none \
             when there are none. -->\n\n## Changes\n<!-- The concrete changes: files, types and \
             behavior. -->\n\n## Progress\n<!-- Append-only, newest last: dated bullets with \
             commit refs. -->\n\n## Verification\n<!-- What proves the task is done. -->\n"
        );
    }

    #[test]
    fn the_shipped_trial_is_d1_to_d5() {
        let kinds = shipped();
        let trial = kinds.get("trial").unwrap();
        assert_eq!(trial.statuses, ["open", "running", "concluded", "dropped"]);
        assert!(trial.allows("open", "running") && trial.allows("running", "concluded"));
        assert!(!trial.allows("concluded", "running") && trial.terminal("dropped"));
        assert_eq!(trial.prefix.as_deref(), Some("trial"));
        let artifact = trial.scaffold("trial-001-x", "2026-11-26");
        for field in ["subject", "rig", "branch"] {
            assert_eq!(artifact.fields[field], Value::String(String::new()), "{field}");
        }
        let headings: Vec<&str> = trial.body.iter().map(|section| section.heading.as_str()).collect();
        assert_eq!(headings, ["Goal", "Log", "Measurements", "Patches", "Conclusion", "Open questions"]);
        assert_eq!(trial.append_only().collect::<Vec<_>>(), ["Log"]);
        assert_eq!(trial.range_field(), None);
        // The scaffold parses as a file of its kind.
        let text = crate::forge::render(&artifact);
        let parsed = crate::forge::parse_artifact(Path::new("/s/trial-001-x.md"), &text, &kinds).unwrap();
        assert_eq!(parsed.fields, artifact.fields);
    }

    #[test]
    fn a_field_takes_values_of_its_kind() {
        let dir = TempDir::new();
        let kind = described(
            dir.path(),
            "x",
            r#"{ statuses = [ "a" ]; initial = "a";
                 fields = {
                   n = { kind = "number"; description = "n"; };
                   when = { kind = "date"; description = "w"; };
                   mood = { kind = "choice"; choices = [ "fine" "bad" ]; description = "m"; };
                   tags = { kind = "list"; of = "string"; description = "t"; };
                   at = { kind = "commit"; required = true; description = "c"; };
                 }; }"#,
        )
        .unwrap();
        let take = |text: &str| {
            let value: Value = serde_json::from_str(text).unwrap();
            let Value::Object(map) = value else { unreachable!() };
            kind.take_fields(Path::new("/s/x.md"), map)
        };
        assert!(take(r#"{ "n": 3, "when": "2026-11-26", "mood": "fine", "tags": ["a"], "at": "" }"#).is_ok());
        assert!(matches!(
            take(r#"{ "mood": "meh", "at": "" }"#),
            Err(Error::Forge(ForgeError::FieldChoice { key, .. })) if key == "mood"
        ));
        assert!(matches!(
            take(r#"{ "when": "2026-02-30", "at": "" }"#),
            Err(Error::Forge(ForgeError::ArtifactDate { key, .. })) if key == "when"
        ));
        assert!(matches!(
            take(r#"{ "tags": [1], "at": "" }"#),
            Err(Error::Common(agentlibre_common::error::Error::Nix(nix::Error::WrongType { key, .. }))) if key == "tags[0]"
        ));
        assert!(matches!(
            take(r#"{ "n": 3 }"#),
            Err(Error::Common(agentlibre_common::error::Error::Nix(nix::Error::MissingKey { key, .. }))) if key == "at"
        ));
        assert!(matches!(
            take(r#"{ "at": "", "other": 1 }"#),
            Err(Error::Forge(ForgeError::ArtifactKey { key, .. })) if key == "other"
        ));
    }

    #[test]
    fn a_broken_description_says_what_is_wrong() {
        // AGL-124, D18.
        assert!(matches!(fault_of("[ 1 ]"), KindFault::NotASet));
        assert!(matches!(fault_of(r#"{ statuses = [ ]; initial = "a"; }"#), KindFault::NoStatuses));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" "a" ]; initial = "a"; }"#),
            KindFault::DuplicateStatus { status } if status == "a"
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; edges = { a = [ "z" ]; }; }"#),
            KindFault::UnknownStatus { key, status } if key == "edges.a" && status == "z"
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; actions = { edit = [ "z" ]; }; }"#),
            KindFault::UnknownStatus { key, .. } if key == "actions.edit"
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; fields = { x = { kind = "blob"; description = ""; }; }; }"#),
            KindFault::FieldKind { kind, .. } if kind == "blob"
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; fields = { x = { kind = "choice"; description = ""; }; }; }"#),
            KindFault::NoChoices { .. }
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; fields = { x = { kind = "number"; default = "1"; description = ""; }; }; }"#),
            KindFault::Default { field, .. } if field == "x"
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; fields = {
                r1 = { kind = "range"; description = ""; }; r2 = { kind = "range"; description = ""; }; }; }"#),
            KindFault::TwoRanges { .. }
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; fields = { status = { kind = "string"; description = ""; }; }; }"#),
            KindFault::FieldName { field } if field == "status"
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; fields = {
                v = { kind = "string"; description = ""; }; "v.w" = { kind = "string"; description = ""; }; }; }"#),
            KindFault::FieldNested { field, .. } if field == "v"
        ));
        assert!(matches!(
            fault_of(r#"{ statuses = [ "a" ]; initial = "a"; prefix = "t1"; }"#),
            KindFault::Prefix { .. }
        ));
        // An unknown key is the strict reading's error, naming the file.
        let dir = TempDir::new();
        assert!(matches!(
            described(dir.path(), "x", r#"{ statuses = [ "a" ]; initial = "a"; extra = 1; }"#),
            Err(Error::Common(agentlibre_common::error::Error::Nix(nix::Error::UnknownKey { key, .. }))) if key == "extra"
        ));
        assert!(described(dir.path(), "x", MINIMAL).is_ok());
    }

    #[test]
    fn a_store_s_descriptions_do_not_reach_another_store() {
        // AGL-124, D20: a project's descriptions are not a plane's.
        let dir = TempDir::new();
        let project = dir.path().join("project");
        let plane = dir.path().join("planes/work");
        fs::create_dir_all(project.join(DIR)).unwrap();
        fs::create_dir_all(&plane).unwrap();
        fs::write(project.join(DIR).join("trial.nix"), MINIMAL).unwrap();
        assert_eq!(Kinds::of(&project).get("trial").unwrap().statuses, ["a", "b"]);
        assert_eq!(Kinds::of(&plane).get("trial").unwrap().statuses, ["open", "running", "concluded", "dropped"]);
    }

    #[test]
    fn the_first_description_found_wins_and_a_broken_one_stays_its_own() {
        // AGL-124, D11, D18, D20: the store, then the config, then agl's.
        let dir = TempDir::new();
        let store = dir.path().join("store");
        let config = dir.path().join("config");
        fs::create_dir_all(&store).unwrap();
        fs::create_dir_all(&config).unwrap();
        let shipped = nix::nix_dir().unwrap().join(DIR);
        fs::write(store.join("trial.nix"), r#"{ statuses = [ "mine" ]; initial = "mine"; }"#).unwrap();
        fs::write(config.join("trial.nix"), r#"{ statuses = [ "config" ]; initial = "config"; }"#).unwrap();
        fs::write(config.join("note.nix"), "{ statuses = ; }").unwrap();
        let kinds = Kinds::in_dirs(&[store.clone(), config.clone(), shipped.clone()]);
        assert_eq!(kinds.get("trial").unwrap().statuses, ["mine"]);
        assert_eq!(kinds.get("trial").unwrap().path, store.join("trial.nix"));
        // Without the store's, the config's.
        let kinds = Kinds::in_dirs(&[dir.path().join("none"), config.clone(), shipped.clone()]);
        assert_eq!(kinds.get("trial").unwrap().statuses, ["config"]);
        // The broken `note` fails alone, naming its file; the others read.
        assert!(kinds.knows("note"));
        assert!(matches!(
            kinds.get("note"),
            Err(Error::Common(agentlibre_common::error::Error::Nix(nix::Error::Eval { path, .. }))) if path == config.join("note.nix")
        ));
        assert!(kinds.get("task-spec").is_ok());
        // No description at all.
        assert!(matches!(
            kinds.get("doc"),
            Err(Error::Forge(ForgeError::UnknownKind { type_name })) if type_name == "doc"
        ));
    }
}
