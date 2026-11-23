//! What can go wrong with a plane: its name, its store, its files and its
//! directories (AGL-077; AGL-121, D66).

use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

/// A plane's own error, wrapped by the core's `Error::Plane` (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// A plane's name that cannot be its store's directory (AGL-077, D5).
    Name { name: String },
    /// `agl plane delete` with no terminal to ask on and no `--yes` (D14).
    DeleteNeedsYes { name: String },
    /// A plane of that name exists (AGL-077, D5).
    Exists { name: String },
    /// No plane has that name (AGL-077).
    NotFound { name: String },
    /// A plane's file (`plane.nix`, `flow.nix`, a journal) is not what agl
    /// reads (AGL-077, D16).
    File { path: PathBuf, detail: String },
    /// `agl plane remove` of a directory the plane does not have.
    DirectoryNotIn { name: String, directory: PathBuf },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Name { name } => tr!(lang, NAME, name = name),
            Self::DeleteNeedsYes { name } => tr!(lang, DELETE_NEEDS_YES, name = name),
            Self::Exists { name } => tr!(lang, EXISTS, name = name),
            Self::NotFound { name } => tr!(lang, NOT_FOUND, name = name),
            Self::File { path, detail } => {
                tr!(lang, FILE, path = path.display(), detail = detail)
            }
            Self::DirectoryNotIn { name, directory } => tr!(
                lang,
                DIRECTORY_NOT_IN,
                name = name,
                directory = directory.display()
            ),
        }
    }
}

/// `Error::Name`.
const NAME: Msg = Msg {
    en: "`{name}` cannot name a plane: a name is not empty, has no `/` and does not start with `.`",
    uk: "`{name}` не може бути назвою plane: назва не порожня, без `/` і не починається з `.`",
};

/// `Error::DeleteNeedsYes`.
const DELETE_NEEDS_YES: Msg = Msg {
    en: "the plane `{name}` is not deleted: there is no terminal to ask on; add --yes",
    uk: "plane `{name}` не видалено: немає термінала, щоб спитати; додай --yes",
};

/// `Error::Exists`.
const EXISTS: Msg = Msg {
    en: "a plane named `{name}` exists",
    uk: "plane з назвою `{name}` уже є",
};

/// `Error::NotFound`.
const NOT_FOUND: Msg = Msg {
    en: "no plane is named `{name}`; `agl plane list` lists them",
    uk: "немає plane з назвою `{name}`; `agl plane list` покаже всі",
};

/// `Error::File`.
const FILE: Msg = Msg {
    en: "{path}: {detail}",
    uk: "{path}: {detail}",
};

/// `Error::DirectoryNotIn`.
const DIRECTORY_NOT_IN: Msg = Msg {
    en: "the plane `{name}` does not have {directory}",
    uk: "у plane `{name}` немає {directory}",
};
