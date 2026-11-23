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
    /// The set of the type of an artifact is missing.
    ArtifactTypeTableMissing { path: PathBuf, type_name: String },
    /// An artifact front-matter key outside the envelope.
    ArtifactKey { path: PathBuf, key: String },
    /// An artifact date is not a `YYYY-MM-DD` calendar date.
    ArtifactDate { path: PathBuf, key: String, value: String },
    /// No artifact is named `name` in the store.
    ArtifactNotFound { name: String, dir: PathBuf },
    /// `forge status` asks for a state outside the lifecycle.
    BadState { state: String },
    /// `forge status` asks for a transition the lifecycle does not
    /// allow.
    BadTransition { from: String, to: String },
    /// `forge status` from a terminal state (`done`, `canceled`, D3 of
    /// AGL-032): the refusal points to the new-spec convention with a
    /// `supersedes` link.
    TerminalState { name: String, from: String },
    /// `forge new` names a type it does not scaffold.
    NewType { type_name: String },
    /// `forge new` names an artifact that is already in the store.
    NameTaken { name: String, dir: PathBuf },
    /// `forge status` on an artifact that is not a `task-spec`.
    NotTaskSpec { name: String, type_name: String },
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
            Self::ArtifactTypeTableMissing { path, type_name } => {
                tr!(
                    lang,
                    ARTIFACT_TYPE_TABLE_MISSING,
                    path = path.display(),
                    type_name = type_name
                )
            }
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
            Self::BadState { state } => tr!(lang, BAD_STATE, state = state),
            Self::BadTransition { from, to } => {
                tr!(lang, BAD_TRANSITION, from = from, to = to)
            }
            Self::TerminalState { name, from } => {
                tr!(lang, TERMINAL_STATE, name = name, from = from)
            }
            Self::NewType { type_name } => tr!(lang, NEW_TYPE, type_name = type_name),
            Self::NameTaken { name, dir } => {
                tr!(lang, NAME_TAKEN, name = name, dir = dir.display())
            }
            Self::NotTaskSpec { name, type_name } => {
                tr!(lang, NOT_TASK_SPEC, name = name, type_name = type_name)
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

/// `Error::ArtifactTypeTableMissing`.
const ARTIFACT_TYPE_TABLE_MISSING: Msg = Msg {
    en: "{path} has no [{type_name}] table",
    uk: "у {path} немає таблиці [{type_name}]",
};

/// `Error::ArtifactUnknownSchema`.
const ARTIFACT_UNKNOWN_SCHEMA: Msg = Msg {
    en: "{path} has schema {schema}, expected forge/v2",
    uk: "у {path} схема {schema}, очікується forge/v2",
};

/// `Error::BadState`.
const BAD_STATE: Msg = Msg {
    en: "{state} is not a task-spec state (open, in-progress, done, backlog, canceled)",
    uk: "{state} не є станом task-spec (open, in-progress, done, backlog, canceled)",
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

/// `Error::NewType`.
const NEW_TYPE: Msg = Msg {
    en: "forge new does not scaffold {type_name}",
    uk: "forge new не створює {type_name}",
};

/// `Error::NotInRepository`.
const NOT_IN_REPOSITORY: Msg = Msg {
    en: "{cwd} is not in a git repository; the forge works on the workspace of the current directory",
    uk: "{cwd} не є git-репозиторієм; forge працює з робочою областю поточної теки",
};

/// `Error::NotTaskSpec`.
const NOT_TASK_SPEC: Msg = Msg {
    en: "{name} is a {type_name}, not a task-spec; status applies to task-specs only",
    uk: "{name} — це {type_name}, а не task-spec; status стосується лише task-spec",
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
