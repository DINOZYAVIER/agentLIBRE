//! What can go wrong in the forge: git, the workspace's markers and
//! stores, an artifact's file, front matter and states (AGL-121, D66).

use std::io;
use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg, m};
use agentlibre_common::tr;

/// The forge's own error, wrapped by the core's `Error::Forge`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// `git` cannot be run.
    GitSpawn { source: io::Error },
    /// `git <args>` exited with a failure.
    GitFailed { args: String, detail: String },
    /// The cwd is not in a git repository; the forge works on the
    /// workspace of the current directory.
    NotInRepository { cwd: PathBuf },
    /// The store directory is not itself a git repository (the
    /// toplevel); `forge log` and `forge diff` need it to be.
    StoreNotRepository { dir: PathBuf },
    /// The `format` of a workspace marker is not 1.
    MarkerFormat { path: PathBuf, format: i64 },
    /// A workspace marker lists a value that is not a `sha256:<hex>`
    /// identity.
    MarkerIdentity { path: PathBuf, value: String },
    /// The marker's `project` is not an absolute path (AGL-055).
    MarkerProject { path: PathBuf, project: PathBuf },
    /// The current identity is in the markers of two stores: one was
    /// copied by hand.
    WorkspaceAmbiguous { dirs: Vec<String> },
    /// `forge rehome` without a store off a TTY.
    RehomeNoStore { stores: Vec<String> },
    /// The cwd is agl's data directory or inside it (AGL-054).
    InsideDataDir { dir: PathBuf, data: PathBuf },
    /// No store is named `name`.
    StoreNotFound { name: String, stores: Vec<String> },
    /// An artifact file has no `---` front-matter delimiters.
    ArtifactFrontMatter { path: PathBuf },
    /// An artifact file carries a schema the forge does not parse.
    ArtifactUnknownSchema { path: PathBuf, schema: String },
    /// An artifact front-matter key outside the envelope.
    ArtifactKey { path: PathBuf, key: String },
    /// An artifact date is not a `YYYY-MM-DD` calendar date.
    ArtifactDate { path: PathBuf, key: String, value: String },
    /// No artifact is named `name` in the store.
    ArtifactNotFound { name: String, dir: PathBuf },
    /// `forge status` asks for a state outside the lifecycle.
    BadState { state: String, type_name: String, statuses: Vec<String> },
    /// `forge status` asks for a transition the lifecycle does not
    /// allow.
    BadTransition { from: String, to: String },
    /// `forge status` from a terminal state (`done`, `canceled`, D3 of
    /// AGL-032): the refusal points to the new-spec convention with a
    /// `supersedes` link.
    TerminalState { name: String, from: String },
    /// `forge new` names an artifact that is already in the store.
    NameTaken { name: String, dir: PathBuf },
    /// No description of the kind `type_name` is found (AGL-124, D11, D18).
    UnknownKind { type_name: String },
    /// The description of a kind is wrong (AGL-124, D18).
    KindDescription { path: PathBuf, fault: KindFault },
    /// A field of kind `choice` holds a value outside its set (AGL-124, D8).
    FieldChoice { path: PathBuf, key: String, value: String, choices: Vec<String> },
    /// `forge range` on a kind whose description has no `range` field
    /// (AGL-124, D16).
    NoRange { name: String, type_name: String },
    /// `forge rehome` while the identity is already in a store: putting
    /// it in a second one is the two-markers trap.
    RehomeInUse { current: String, target: String },
    /// A verification cmd of `forge check` cannot be run.
    CmdSpawn { source: io::Error },
    /// A task-spec id does not resolve in the workspace repository.
    CommitNotFound { id: String, repo: PathBuf },
    /// An artifact file cannot be removed.
    Remove { path: PathBuf, source: io::Error },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::GitSpawn { source } => tr!(lang, GIT_SPAWN, source = source),
            Self::GitFailed { args, detail } => {
                tr!(lang, GIT_FAILED, args = args, detail = detail)
            }
            Self::NotInRepository { cwd } => {
                tr!(lang, NOT_IN_REPOSITORY, cwd = cwd.display())
            }
            Self::StoreNotRepository { dir } => {
                tr!(lang, STORE_NOT_REPOSITORY, dir = dir.display())
            }
            Self::MarkerFormat { path, format } => {
                tr!(lang, MARKER_FORMAT, path = path.display(), format = format)
            }
            Self::MarkerIdentity { path, value } => {
                tr!(lang, MARKER_IDENTITY, path = path.display(), value = value)
            }
            Self::MarkerProject { path, project } => {
                tr!(lang, MARKER_PROJECT, path = path.display(), project = project.display())
            }
            Self::WorkspaceAmbiguous { dirs } => {
                tr!(lang, WORKSPACE_AMBIGUOUS, dirs = dirs.join(", "))
            }
            Self::RehomeNoStore { stores } => {
                let stores = if stores.is_empty() {
                    "none".to_string()
                } else {
                    stores.join(", ")
                };
                tr!(lang, REHOME_NO_STORE, stores = stores)
            }
            Self::InsideDataDir { dir, data } => {
                tr!(lang, INSIDE_DATA_DIR, dir = dir.display(), data = data.display())
            }
            Self::StoreNotFound { name, stores } => {
                let stores = if stores.is_empty() {
                    "none".to_string()
                } else {
                    stores.join(", ")
                };
                tr!(lang, STORE_NOT_FOUND, name = name, stores = stores)
            }
            Self::ArtifactFrontMatter { path } => {
                tr!(lang, ARTIFACT_FRONT_MATTER, path = path.display())
            }
            Self::ArtifactUnknownSchema { path, schema } => {
                tr!(lang, ARTIFACT_UNKNOWN_SCHEMA, path = path.display(), schema = schema)
            }
            Self::UnknownKind { type_name } => tr!(lang, UNKNOWN_KIND, type_name = type_name),
            Self::KindDescription { path, fault } => {
                tr!(lang, KIND_DESCRIPTION, path = path.display(), fault = fault.message(lang))
            }
            Self::FieldChoice { path, key, value, choices } => tr!(
                lang,
                FIELD_CHOICE,
                path = path.display(),
                key = key,
                value = value,
                choices = choices.join(", ")
            ),
            Self::ArtifactKey { path, key } => {
                tr!(lang, ARTIFACT_KEY, path = path.display(), key = key)
            }
            Self::ArtifactDate { path, key, value } => {
                tr!(
                    lang,
                    ARTIFACT_DATE,
                    path = path.display(),
                    key = key,
                    value = value
                )
            }
            Self::ArtifactNotFound { name, dir } => {
                tr!(
                    lang,
                    ARTIFACT_NOT_FOUND,
                    name = name,
                    dir = dir.display()
                )
            }
            Self::BadState { state, type_name, statuses } => {
                tr!(lang, BAD_STATE, state = state, type_name = type_name, statuses = statuses.join(", "))
            }
            Self::BadTransition { from, to } => {
                tr!(lang, BAD_TRANSITION, from = from, to = to)
            }
            Self::TerminalState { name, from } => {
                tr!(lang, TERMINAL_STATE, name = name, from = from)
            }
            Self::NameTaken { name, dir } => {
                tr!(lang, NAME_TAKEN, name = name, dir = dir.display())
            }
            Self::NoRange { name, type_name } => {
                tr!(lang, NO_RANGE, name = name, type_name = type_name)
            }
            Self::RehomeInUse { current, target } => {
                tr!(lang, REHOME_IN_USE, current = current, target = target)
            }
            Self::CmdSpawn { source } => tr!(lang, CMD_SPAWN, source = source),
            Self::CommitNotFound { id, repo } => {
                tr!(lang, m::COMMIT_NOT_FOUND, id = id, repo = repo.display())
            }
            Self::Remove { path, source } => {
                tr!(lang, REMOVE, path = path.display(), source = source)
            }
        }
    }
}

/// `Error::ArtifactDate`.
const ARTIFACT_DATE: Msg = Msg {
    en: "{path} has {key} = {value}, not a YYYY-MM-DD date",
    uk: "у {path} {key} = {value}, а не дата YYYY-MM-DD",
};

/// `Error::ArtifactFrontMatter`.
const ARTIFACT_FRONT_MATTER: Msg = Msg {
    en: "{path} has no `---` front-matter",
    uk: "у {path} немає `---` заголовка",
};

/// `Error::ArtifactKey`.
const ARTIFACT_KEY: Msg = Msg {
    en: "{path} has an unknown key `{key}`",
    uk: "у {path} невідомий ключ `{key}`",
};

/// `Error::ArtifactNotFound`.
const ARTIFACT_NOT_FOUND: Msg = Msg {
    en: "no artifact is named {name} in {dir}",
    uk: "артефакту на ім'я {name} немає в {dir}",
};

/// `Error::UnknownKind`.
const UNKNOWN_KIND: Msg = Msg {
    en: "no description of the kind {type_name} is found: forge-types/{type_name}.nix is in none of the store, the user's config or agl's",
    uk: "опису виду {type_name} не знайдено: forge-types/{type_name}.nix немає ні в сховищі, ні в конфігурації користувача, ні в agl",
};

/// `Error::KindDescription`.
const KIND_DESCRIPTION: Msg = Msg {
    en: "the description {path} is wrong: {fault}",
    uk: "опис {path} хибний: {fault}",
};

/// `Error::FieldChoice`.
const FIELD_CHOICE: Msg = Msg {
    en: "`{key}` in {path} is {value}; it must be one of: {choices}",
    uk: "`{key}` у {path} — {value}; має бути одним із: {choices}",
};

/// `Error::ArtifactUnknownSchema`.
const ARTIFACT_UNKNOWN_SCHEMA: Msg = Msg {
    en: "{path} has schema {schema}, expected forge/v2",
    uk: "у {path} схема {schema}, очікується forge/v2",
};

/// `Error::BadState`.
const BAD_STATE: Msg = Msg {
    en: "{state} is not a {type_name} state ({statuses})",
    uk: "{state} не є станом {type_name} ({statuses})",
};

/// `Error::BadTransition`.
const BAD_TRANSITION: Msg = Msg {
    en: "a task-spec cannot go from {from} to {to}",
    uk: "task-spec не може перейти з {from} у {to}",
};

/// `Error::CmdSpawn`.
const CMD_SPAWN: Msg = Msg {
    en: "the check command cannot be run: {source}",
    uk: "команду перевірки не можна запустити: {source}",
};

/// `Error::GitFailed`.
const GIT_FAILED: Msg = Msg {
    en: "git {args} failed: {detail}",
    uk: "git {args} завершився з помилкою: {detail}",
};

/// `Error::GitSpawn`.
const GIT_SPAWN: Msg = Msg {
    en: "cannot run git: {source}",
    uk: "не вдалося запустити git: {source}",
};

/// `Error::InsideDataDir`.
const INSIDE_DATA_DIR: Msg = Msg {
    en: "{dir} is inside agl's data directory {data}, which is not a project: run agl from a project",
    uk: "{dir} лежить у каталозі даних agl {data}, а це не проєкт: запускай agl із проєкту",
};

/// `Error::MarkerFormat`.
const MARKER_FORMAT: Msg = Msg {
    en: "{path}: the marker format is {format}, expected 1",
    uk: "{path}: формат маркера {format}, очікується 1",
};

/// `Error::MarkerIdentity`.
const MARKER_IDENTITY: Msg = Msg {
    en: "{path} lists {value}, which is not a sha256 identity",
    uk: "{path} містить {value}, а це не sha256-ідентифікатор",
};

/// `Error::MarkerProject`.
const MARKER_PROJECT: Msg = Msg {
    en: "{path} names the project {project}, which is not an absolute path",
    uk: "{path} називає проєкт {project}, а це не абсолютний шлях",
};

/// `Error::NameTaken`.
const NAME_TAKEN: Msg = Msg {
    en: "an artifact named {name} is already in {dir}",
    uk: "артефакт на ім'я {name} вже є в {dir}",
};


/// `Error::NotInRepository`.
const NOT_IN_REPOSITORY: Msg = Msg {
    en: "{cwd} is not in a git repository; the forge works on the workspace of the current directory",
    uk: "{cwd} не є git-репозиторієм; forge працює з робочою областю поточної теки",
};

/// `Error::NoRange`.
const NO_RANGE: Msg = Msg {
    en: "{name} is a {type_name}, whose description has no range field; range applies to the kinds that have one",
    uk: "{name} — це {type_name}, опис якого не має поля range; range стосується видів, що його мають",
};

/// `Error::RehomeInUse`.
const REHOME_IN_USE: Msg = Msg {
    en: "the identity is already in the store {current}; it cannot also be in {target}",
    uk: "ідентичність вже є в сховищі {current}; її не можна ще й в {target}",
};

/// `Error::RehomeNoStore`.
const REHOME_NO_STORE: Msg = Msg {
    en: "a store cannot be chosen without a terminal; stores: {stores} (run `agl forge rehome <store>`)",
    uk: "без термінала не можна вибрати сховище; сховища: {stores} (запусти `agl forge rehome <store>`)",
};

/// `Error::Remove`.
const REMOVE: Msg = Msg {
    en: "cannot remove {path}: {source}",
    uk: "не вдалося прибрати {path}: {source}",
};

/// `Error::StoreNotFound`.
const STORE_NOT_FOUND: Msg = Msg {
    en: "no store is named {name}; stores: {stores}",
    uk: "немає сховища на ім'я {name}; сховища: {stores}",
};

/// `Error::StoreNotRepository`.
const STORE_NOT_REPOSITORY: Msg = Msg {
    en: "the store directory is not a git repository: {dir}",
    uk: "каталог сховища не є git-репозиторієм: {dir}",
};

/// `Error::TerminalState` (D3 of AGL-032): the refusal points to the
/// new-spec convention with a `supersedes` link.
const TERMINAL_STATE: Msg = Msg {
    en: "{name} is {from}; {from} is a terminal state — to continue the work, make a new spec with `supersedes = [{name}]`",
    uk: "{name} — {from}; {from} — термінальний стан — щоб продовжити роботу, створіть нову спеку з `supersedes = [{name}]`",
};

/// `Error::WorkspaceAmbiguous`.
const WORKSPACE_AMBIGUOUS: Msg = Msg {
    en: "the workspace identity is in the markers of {dirs}; a store was copied by hand, remove the copy",
    uk: "ідентифікатор робочої області є в маркерах {dirs}; сховище скопійовано вручну, прибери копію",
};

/// What is wrong in the description of a kind (AGL-124, D18).
#[derive(Debug)]
pub enum KindFault {
    /// The file does not evaluate to an attribute set (D22).
    NotASet,
    /// `statuses` is empty.
    NoStatuses,
    /// A status is listed twice.
    DuplicateStatus { status: String },
    /// `key` names a status that is not in `statuses`.
    UnknownStatus { key: String, status: String },
    /// A field's `kind` is none of the twelve (D8).
    FieldKind { field: String, kind: String },
    /// A field of kind `choice` lists no `choices`.
    NoChoices { field: String },
    /// A `list` field's `of` is not a plain kind.
    ListOf { field: String, of: String },
    /// A field carries `choices` or `of` that its kind does not take.
    Stray { field: String, key: String },
    /// A field's `default` is not of its kind.
    Default { field: String, kind: String },
    /// A field's name is empty in a part, or one of the envelope's (D7).
    FieldName { field: String },
    /// A field's name is the set of another field.
    FieldNested { field: String, inner: String },
    /// Two fields of kind `range` (D16).
    TwoRanges { first: String, second: String },
    /// A section heading is empty or more than a line.
    Heading { heading: String },
    /// A section heading is listed twice.
    DuplicateHeading { heading: String },
    /// `prefix` is not a run of letters (D15).
    Prefix { prefix: String },
}

impl KindFault {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NotASet => tr!(lang, FAULT_NOT_A_SET).to_string(),
            Self::NoStatuses => tr!(lang, FAULT_NO_STATUSES).to_string(),
            Self::DuplicateStatus { status } => tr!(lang, FAULT_DUPLICATE_STATUS, status = status),
            Self::UnknownStatus { key, status } => tr!(lang, FAULT_UNKNOWN_STATUS, key = key, status = status),
            Self::FieldKind { field, kind } => tr!(lang, FAULT_FIELD_KIND, field = field, kind = kind),
            Self::NoChoices { field } => tr!(lang, FAULT_NO_CHOICES, field = field),
            Self::ListOf { field, of } => tr!(lang, FAULT_LIST_OF, field = field, of = of),
            Self::Stray { field, key } => tr!(lang, FAULT_STRAY, field = field, key = key),
            Self::Default { field, kind } => tr!(lang, FAULT_DEFAULT, field = field, kind = kind),
            Self::FieldName { field } => tr!(lang, FAULT_FIELD_NAME, field = field),
            Self::FieldNested { field, inner } => tr!(lang, FAULT_FIELD_NESTED, field = field, inner = inner),
            Self::TwoRanges { first, second } => tr!(lang, FAULT_TWO_RANGES, first = first, second = second),
            Self::Heading { heading } => tr!(lang, FAULT_HEADING, heading = heading),
            Self::DuplicateHeading { heading } => tr!(lang, FAULT_DUPLICATE_HEADING, heading = heading),
            Self::Prefix { prefix } => tr!(lang, FAULT_PREFIX, prefix = prefix),
        }
    }
}

/// `KindFault::NotASet`.
const FAULT_NOT_A_SET: Msg = Msg {
    en: "it is not an attribute set",
    uk: "це не набір атрибутів",
};

/// `KindFault::NoStatuses`.
const FAULT_NO_STATUSES: Msg = Msg {
    en: "`statuses` is empty",
    uk: "`statuses` порожній",
};

/// `KindFault::DuplicateStatus`.
const FAULT_DUPLICATE_STATUS: Msg = Msg {
    en: "the status {status} is listed twice in `statuses`",
    uk: "статус {status} двічі в `statuses`",
};

/// `KindFault::UnknownStatus`.
const FAULT_UNKNOWN_STATUS: Msg = Msg {
    en: "`{key}` names {status}, which is not in `statuses`",
    uk: "`{key}` називає {status}, якого немає в `statuses`",
};

/// `KindFault::FieldKind`.
const FAULT_FIELD_KIND: Msg = Msg {
    en: "the field {field} is of the kind {kind}; the kinds are string, text, number, bool, date, choice, list, ref, refs, commit, range and cmds",
    uk: "поле {field} має вид {kind}; види: string, text, number, bool, date, choice, list, ref, refs, commit, range і cmds",
};

/// `KindFault::NoChoices`.
const FAULT_NO_CHOICES: Msg = Msg {
    en: "the field {field} is a choice with no `choices`",
    uk: "поле {field} — choice без `choices`",
};

/// `KindFault::ListOf`.
const FAULT_LIST_OF: Msg = Msg {
    en: "the list {field} is of `{of}`; `of` is string, text, number, bool, date, choice, ref or commit",
    uk: "список {field} має `of` = `{of}`; `of` — string, text, number, bool, date, choice, ref або commit",
};

/// `KindFault::Stray`.
const FAULT_STRAY: Msg = Msg {
    en: "the field {field} has `{key}`, which its kind does not take",
    uk: "поле {field} має `{key}`, якого його вид не бере",
};

/// `KindFault::Default`.
const FAULT_DEFAULT: Msg = Msg {
    en: "the default of the field {field} is not a {kind}",
    uk: "типове значення поля {field} — не {kind}",
};

/// `KindFault::FieldName`.
const FAULT_FIELD_NAME: Msg = Msg {
    en: "the field name {field} is empty in a part or one of the envelope's (schema, type, name, status, created, updated)",
    uk: "ім'я поля {field} має порожню частину або належить конверту (schema, type, name, status, created, updated)",
};

/// `KindFault::FieldNested`.
const FAULT_FIELD_NESTED: Msg = Msg {
    en: "the field {field} is the set of the field {inner}",
    uk: "поле {field} є набором поля {inner}",
};

/// `KindFault::TwoRanges`.
const FAULT_TWO_RANGES: Msg = Msg {
    en: "two fields are of the kind range, {first} and {second}; a kind has one at most",
    uk: "два поля мають вид range, {first} і {second}; у виду їх щонайбільше одне",
};

/// `KindFault::Heading`.
const FAULT_HEADING: Msg = Msg {
    en: "the section heading `{heading}` is empty or more than a line",
    uk: "заголовок секції `{heading}` порожній або довший за рядок",
};

/// `KindFault::DuplicateHeading`.
const FAULT_DUPLICATE_HEADING: Msg = Msg {
    en: "the section {heading} is listed twice in `body`",
    uk: "секція {heading} двічі в `body`",
};

/// `KindFault::Prefix`.
const FAULT_PREFIX: Msg = Msg {
    en: "the prefix `{prefix}` is not a run of letters",
    uk: "префікс `{prefix}` — не послідовність літер",
};
