//! What can go wrong with a session: finding it, naming it, reading and
//! writing its file (AGL-121, D66).

use std::io;
use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

/// A session's own error, wrapped by the core's `Error::Session`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// No session in `dir` has an id that is or starts with `id`.
    NotFound { id: String, dir: PathBuf },
    /// More than one session id starts with `id` (D107).
    Ambiguous { id: String, count: usize },
    /// `/rename` to a text that is no name (AGL-062, D6).
    Name { problem: NameProblem },
    /// `/rename` to a name that another session of the project has
    /// (AGL-062, D5).
    NameTaken { name: String, id: String },
    /// Line `line` of a session file is not a valid event.
    Line {
        path: PathBuf,
        line: usize,
        detail: String,
    },
    /// A session file cannot be written.
    Write { path: PathBuf, source: io::Error },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NotFound { id, dir } => {
                tr!(lang, SESSION_NOT_FOUND, id = id, dir = dir.display())
            }
            Self::Ambiguous { id, count } => tr!(lang, SESSION_AMBIGUOUS, id = id, count = count),
            Self::Name { problem } => match problem {
                NameProblem::Empty => tr!(lang, SESSION_NAME_EMPTY).to_string(),
                NameProblem::Lines => tr!(lang, SESSION_NAME_LINES).to_string(),
                NameProblem::TooLong => tr!(lang, SESSION_NAME_LONG, max = super::NAME_MAX),
                NameProblem::Reserved => tr!(lang, SESSION_NAME_RESERVED).to_string(),
            },
            Self::NameTaken { name, id } => tr!(lang, SESSION_NAME_TAKEN, name = name, id = id),
            Self::Line { path, line, detail } => tr!(
                lang,
                SESSION_LINE,
                path = path.display(),
                line = line,
                detail = detail
            ),
            Self::Write { path, source } => {
                tr!(lang, SESSION_WRITE, path = path.display(), source = source)
            }
        }
    }
}

/// Why a text is no name of a session (AGL-062, D6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameProblem {
    /// Nothing is left after trimming.
    Empty,
    /// It has more than one line.
    Lines,
    /// It is longer than `session::NAME_MAX`.
    TooLong,
    /// It is `last`, which `--resume` reads as the newest session.
    Reserved,
}

/// `Error::NotFound`.
const SESSION_NOT_FOUND: Msg = Msg {
    en: "no session in {dir} is named `{id}` or has an id that starts with it",
    uk: "у {dir} немає сесії з іменем `{id}` чи з id, що з нього починається",
};

/// `Error::Name` (AGL-062, D6).
const SESSION_NAME_EMPTY: Msg = Msg {
    en: "a name cannot be empty",
    uk: "ім'я не може бути порожнім",
};

const SESSION_NAME_LINES: Msg = Msg {
    en: "a name is one line",
    uk: "ім'я — один рядок",
};

const SESSION_NAME_LONG: Msg = Msg {
    en: "a name has at most {max} characters",
    uk: "ім'я має щонайбільше {max} символів",
};

const SESSION_NAME_RESERVED: Msg = Msg {
    en: "`last` is no name: `--resume last` is the newest session",
    uk: "`last` не може бути іменем: `--resume last` — найновіша сесія",
};

/// `Error::NameTaken` (AGL-062, D5).
const SESSION_NAME_TAKEN: Msg = Msg {
    en: "the session {id} of this project is named `{name}` already",
    uk: "сесія {id} цього проєкту вже має ім'я `{name}`",
};

/// `Error::Ambiguous`.
const SESSION_AMBIGUOUS: Msg = Msg {
    en: "{count} sessions start with `{id}`; give more of the id",
    uk: "сесій, що починаються з `{id}`: {count}; вкажи довшу частину id",
};

/// `Error::Line`.
const SESSION_LINE: Msg = Msg {
    en: "line {line} of {path}: {detail}",
    uk: "рядок {line} у {path}: {detail}",
};

/// `Error::Write`.
const SESSION_WRITE: Msg = Msg {
    en: "cannot write {path}: {source}",
    uk: "не вдалося записати {path}: {source}",
};
