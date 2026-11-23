//! What is wrong with a value of the settings: a style, a limit, a port,
//! a language, a url, a level of detail, a path (AGL-121, D66).

use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg};
use crate::style::StyleError;
use agentlibre_common::tr;

/// The settings' own error, wrapped by the core's `Error::Settings`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// The file of `private_ca` cannot be used (AGL-058, D12; AGL-121,
    /// D78).
    ServerCa { path: PathBuf, detail: String },
    /// A value of `colors` is not a valid style (D121).
    InvalidStyle {
        path: PathBuf,
        key: String,
        value: String,
        problem: StyleError,
    },
    /// A limit of `repl` below 1 (D130).
    NotPositive { path: PathBuf, key: String },
    /// `bridge.port` is not 1 to 65535 (AGL-083, D20).
    NotAPort { path: PathBuf, key: String, value: i64 },
    /// `language` names a language without translations.
    UnsupportedLanguage { path: PathBuf, value: String },
    /// `server.llama.url` or `server.strata.url` is not an `http://` or
    /// `https://` URL (D52; AGL-036, D2; AGL-043 F10).
    UnsupportedUrl { path: PathBuf, url: String },
    /// `repl.thinking`, `repl.tool` or `repl.result` is not `off`,
    /// `flexible` or `full` (AGL-092, D2; AGL-095, D1).
    UnknownDetailLevel {
        path: PathBuf,
        key: String,
        value: String,
    },
    /// `search.url` is not an `https://` URL (AGL-019, D3).
    UnsupportedSearchUrl { path: PathBuf, url: String },
    /// A file path of `search` is not absolute (AGL-019, D3).
    SearchPathNotAbsolute {
        path: PathBuf,
        key: String,
        value: String,
    },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::ServerCa { path, detail } => {
                tr!(lang, SERVER_CA, path = path.display(), detail = detail)
            }
            Self::InvalidStyle {
                path,
                key,
                value,
                problem,
            } => {
                tr!(
                    lang,
                    INVALID_STYLE,
                    path = path.display(),
                    key = key,
                    value = value,
                    problem = problem.message(lang)
                )
            }
            Self::NotPositive { path, key } => {
                tr!(lang, NOT_POSITIVE, path = path.display(), key = key)
            }
            Self::NotAPort { path, key, value } => {
                tr!(lang, NOT_A_PORT, path = path.display(), key = key, value = value)
            }
            Self::UnsupportedLanguage { path, value } => {
                tr!(
                    lang,
                    UNSUPPORTED_LANGUAGE,
                    path = path.display(),
                    value = value
                )
            }
            Self::UnsupportedUrl { path, url } => {
                tr!(lang, UNSUPPORTED_URL, path = path.display(), url = url)
            }
            Self::UnknownDetailLevel { path, key, value } => tr!(
                lang,
                UNKNOWN_DETAIL_LEVEL,
                path = path.display(),
                key = key,
                value = value
            ),
            Self::UnsupportedSearchUrl { path, url } => {
                tr!(lang, UNSUPPORTED_SEARCH_URL, path = path.display(), url = url)
            }
            Self::SearchPathNotAbsolute { path, key, value } => {
                tr!(
                    lang,
                    SEARCH_PATH_NOT_ABSOLUTE,
                    path = path.display(),
                    key = key,
                    value = value
                )
            }
        }
    }
}

/// `Error::InvalidStyle`.
const INVALID_STYLE: Msg = Msg {
    en: "invalid style `{value}` for `{key}` in {path}: {problem}",
    uk: "некоректний стиль `{value}` для `{key}` у {path}: {problem}",
};

/// `Error::NotPositive`.
const NOT_POSITIVE: Msg = Msg {
    en: "`{key}` in {path} must be at least 1",
    uk: "`{key}` у {path} має бути не менше 1",
};

/// `Error::NotAPort` (AGL-083, D20).
const NOT_A_PORT: Msg = Msg {
    en: "`{key}` in {path} must be a port, 1 to 65535, not {value}",
    uk: "`{key}` у {path} має бути портом від 1 до 65535, а не {value}",
};

/// `Error::UnsupportedLanguage`.
const UNSUPPORTED_LANGUAGE: Msg = Msg {
    en: "unsupported language `{value}` in {path}; supported: en, uk",
    uk: "непідтримувана мова `{value}` у {path}; підтримуються: en, uk",
};

/// `Error::UnsupportedUrl`.
const UNSUPPORTED_URL: Msg = Msg {
    en: "unsupported server address `{url}` in {path}; http:// and https:// are supported",
    uk: "непідтримувана адреса сервера `{url}` у {path}; підтримуються http:// і https://",
};

/// `Error::UnknownDetailLevel` (AGL-092, D2; AGL-095, D1).
const UNKNOWN_DETAIL_LEVEL: Msg = Msg {
    en: "`repl.{key}` in {path} is not `off`, `flexible` or `full`: `{value}`",
    uk: "`repl.{key}` у {path} не `off`, не `flexible` і не `full`: `{value}`",
};

/// `Error::UnsupportedSearchUrl`.
const UNSUPPORTED_SEARCH_URL: Msg = Msg {
    en: "unsupported search address `{url}` in {path}; only https:// is supported",
    uk: "непідтримувана адреса пошуку `{url}` у {path}; підтримується лише https://",
};

/// `Error::SearchPathNotAbsolute`.
const SEARCH_PATH_NOT_ABSOLUTE: Msg = Msg {
    en: "`{key}` in {path} must be an absolute path, not `{value}`",
    uk: "`{key}` у {path} має бути абсолютним шляхом, а не `{value}`",
};

/// `Error::ServerCa` (AGL-058, D12).
const SERVER_CA: Msg = Msg {
    en: "the CA file {path} of `private_ca` cannot be used: {detail}",
    uk: "файл CA {path} із `private_ca` не можна використати: {detail}",
};
