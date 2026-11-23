//! The terminal's errors: the core's, and those only the terminal makes:
//! its command line, the bridge's unit and devices, the search login, the
//! notifier's launch, the current directory and raw mode (AGL-121, D69,
//! D72).

use std::io;
use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg, m};
use agentlibre_common::tr;

/// The core's error.
pub(crate) use agentlibre_core::error::Error as CoreError;

/// An error of the terminal: one of the core's, or one only the terminal
/// makes (AGL-121, D69, D72).
#[derive(Debug)]
pub enum Error {
    /// An error of agl's core.
    Core(CoreError),
    /// `agl bridge` and `bridge` lacks `address` or `port` (AGL-083, D20).
    BridgeNotSet { path: PathBuf },
    /// No `agl-bridge` beside `agl` (AGL-083, D18).
    BridgeProgramMissing { path: PathBuf },
    /// The bridge's unit did not come to listen.
    BridgeNotListening { unit: String },
    /// A device's name the bridge does not take (AGL-083, D10).
    BridgeDeviceName { name: String },
    /// `agl bridge pair` of a device paired already.
    BridgeDeviceExists { name: String },
    /// `agl bridge revoke` of a device not paired.
    BridgeDeviceUnknown { name: String },
    /// The bridge's directory could not be read or written.
    Bridge { detail: String },
    /// `agl search login` without a complete `search.ca` in the user file
    /// (AGL-043 F20); `key` is the first one missing.
    SearchCaNotSet { user_path: PathBuf, key: String },
    /// A step of `agl search login` failed (AGL-043 F20).
    SearchLogin { detail: String },
    /// `agl notify` cannot start the `agl-notify` beside it (AGL-121, D23).
    NotifySpawn { source: io::Error },
    /// An argument is not valid UTF-8.
    InvalidUnicode,
    /// An argument is not a known option.
    UnknownOption(String),
    /// An option is the last argument.
    MissingValue(String),
    /// An option is given twice.
    RepeatedOption(String),
    /// `--think` names a level outside the four (AGL-013).
    BadReasoningLevel { value: String },
    /// The current directory cannot be read.
    CurrentDir(io::Error),
    /// `--attach` without `-p` (AGL-014, D21).
    AttachWithoutPrompt,
    /// The terminal cannot be switched to raw mode.
    Terminal { source: io::Error },
}

impl From<CoreError> for Error {
    fn from(source: CoreError) -> Error {
        Error::Core(source)
    }
}

/// A core module's own error is the core's, and so the terminal's.
macro_rules! from_core {
    ($($module:ident),*) => {
        $(impl From<agentlibre_core::$module::error::Error> for Error {
            fn from(source: agentlibre_core::$module::error::Error) -> Error {
                Error::Core(source.into())
            }
        })*
    };
}

from_core!(backend, daemon, engine, forge, function, machine, plane, session, settings);

/// A core module whose error is `module::Error`.
macro_rules! from_core_file {
    ($($module:ident),*) => {
        $(impl From<agentlibre_core::$module::Error> for Error {
            fn from(source: agentlibre_core::$module::Error) -> Error {
                Error::Core(source.into())
            }
        })*
    };
}

from_core_file!(download, format, media, memory, nix, paths, search, strata, weights, whisper, wsl);

/// A common module's own error, wrapped by the core's.
macro_rules! from_common {
    ($($module:ident),*) => {
        $(impl From<agentlibre_common::$module::Error> for Error {
            fn from(source: agentlibre_common::$module::Error) -> Error {
                Error::Core(source.into())
            }
        })*
    };
}

from_common!(programs);

impl Error {
    /// Whether the error is in the command line rather than the setup:
    /// the terminal's own, or the core's (AGL-121, D76).
    pub fn is_usage(&self) -> bool {
        if let Error::Core(error) = self {
            return error.is_usage();
        }
        matches!(
            self,
            Error::InvalidUnicode
                | Error::UnknownOption(_)
                | Error::MissingValue(_)
                | Error::RepeatedOption(_)
                | Error::BadReasoningLevel { .. }
                | Error::AttachWithoutPrompt
        )
    }

    /// Process exit status for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Core(error) => error.exit_code(),
            _ if self.is_usage() => 2,
            _ => 1,
        }
    }

    /// The message of `self` in `lang`; a usage error points to the help.
    pub fn message(&self, lang: Lang) -> String {
        let message = self.text(lang);
        // The core's message points to the help itself (D76).
        if !self.is_usage() || matches!(self, Error::Core(_)) {
            return message;
        }
        format!("{message}\n{}", tr!(lang, m::USAGE_POINTER))
    }

    fn text(&self, lang: Lang) -> String {
        match self {
            Error::Core(error) => error.message(lang),
            Error::BridgeNotSet { path } => tr!(lang, BRIDGE_NOT_SET, path = path.display()),
            Error::BridgeProgramMissing { path } => {
                tr!(lang, BRIDGE_PROGRAM_MISSING, path = path.display())
            }
            Error::BridgeNotListening { unit } => tr!(lang, BRIDGE_NOT_LISTENING, unit = unit),
            Error::BridgeDeviceName { name } => tr!(lang, BRIDGE_DEVICE_NAME, name = name),
            Error::BridgeDeviceExists { name } => tr!(lang, BRIDGE_DEVICE_EXISTS, name = name),
            Error::BridgeDeviceUnknown { name } => tr!(lang, BRIDGE_DEVICE_UNKNOWN, name = name),
            Error::Bridge { detail } => tr!(lang, BRIDGE_FAILED, detail = detail),
            Error::SearchCaNotSet { user_path, key } => tr!(
                lang,
                SEARCH_CA_NOT_SET,
                user_path = user_path.display(),
                key = key
            ),
            Error::SearchLogin { detail } => tr!(lang, SEARCH_LOGIN, detail = detail),
            Error::NotifySpawn { source } => tr!(lang, NOTIFY_SPAWN, source = source),
            Error::InvalidUnicode => tr!(lang, INVALID_UNICODE).to_string(),
            Error::UnknownOption(arg) => tr!(lang, UNKNOWN_OPTION, arg = arg),
            Error::MissingValue(arg) => tr!(lang, MISSING_VALUE, arg = arg),
            Error::RepeatedOption(arg) => tr!(lang, REPEATED_OPTION, arg = arg),
            Error::BadReasoningLevel { value } => {
                tr!(lang, BAD_REASONING_LEVEL, value = value)
            }
            Error::CurrentDir(source) => tr!(lang, CURRENT_DIR, source = source),
            Error::AttachWithoutPrompt => tr!(lang, ATTACH_WITHOUT_PROMPT).to_string(),
            Error::Terminal { source } => tr!(lang, TERMINAL, source = source),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(Lang::En))
    }
}

/// `Error::AttachWithoutPrompt`.
const ATTACH_WITHOUT_PROMPT: Msg = Msg {
    en: "--attach goes with -p; in a session use /attach <path>",
    uk: "--attach працює разом з -p; у сесії використовуй /attach <шлях>",
};

/// `Error::BadReasoningLevel`.
const BAD_REASONING_LEVEL: Msg = Msg {
    en: "invalid reasoning level `{value}` for `--think`: \
         the levels are off, low, medium and xhigh",
    uk: "некоректний рівень міркувань `{value}` для `--think`: \
         рівні — off, low, medium та xhigh",
};

/// `Error::BridgeDeviceExists`.
const BRIDGE_DEVICE_EXISTS: Msg = Msg {
    en: "the device {name} is paired already; to pair it anew, agl bridge revoke {name} first",
    uk: "пристрій {name} уже спарено; щоб спарити заново, спершу agl bridge revoke {name}",
};

/// `Error::BridgeDeviceName` (AGL-083, D10).
const BRIDGE_DEVICE_NAME: Msg = Msg {
    en: "{name:?} is no device name: 1 to 64 letters, digits, `.`, `_` or `-`",
    uk: "{name:?} не назва пристрою: від 1 до 64 латинських літер, цифр, `.`, `_` або `-`",
};

/// `Error::BridgeDeviceUnknown`.
const BRIDGE_DEVICE_UNKNOWN: Msg = Msg {
    en: "no device {name} is paired (agl bridge list)",
    uk: "пристрою {name} не спарено (agl bridge list)",
};

/// `Error::Bridge`.
const BRIDGE_FAILED: Msg = Msg {
    en: "the bridge: {detail}",
    uk: "міст: {detail}",
};

/// `Error::BridgeNotListening`.
const BRIDGE_NOT_LISTENING: Msg = Msg {
    en: "the bridge did not start listening; see journalctl --user -u {unit}",
    uk: "міст не почав слухати; дивись journalctl --user -u {unit}",
};

/// `Error::BridgeNotSet` (AGL-083, D20).
const BRIDGE_NOT_SET: Msg = Msg {
    en: "the bridge needs `bridge.address` and `bridge.port`; add to {path}, with the address of this computer that WireGuard reaches:\n\nbridge = {{ address = \"10.0.0.5\"; port = 18443; }};",
    uk: "мостові потрібні `bridge.address` і `bridge.port`; додай у {path}, з адресою цього комп'ютера, до якої доходить WireGuard:\n\nbridge = {{ address = \"10.0.0.5\"; port = 18443; }};",
};

/// `Error::BridgeProgramMissing` (AGL-083, D18).
const BRIDGE_PROGRAM_MISSING: Msg = Msg {
    en: "no agl-bridge beside agl: {path}",
    uk: "поруч з agl немає agl-bridge: {path}",
};

/// `Error::CurrentDir`.
const CURRENT_DIR: Msg = Msg {
    en: "cannot read the current directory: {source}",
    uk: "не вдалося визначити поточну теку: {source}",
};

/// `Error::InvalidUnicode`.
const INVALID_UNICODE: Msg = Msg {
    en: "arguments must be valid UTF-8",
    uk: "аргументи мають бути коректним UTF-8",
};

/// `Error::MissingValue`.
const MISSING_VALUE: Msg = Msg {
    en: "option `{arg}` needs a value",
    uk: "параметру `{arg}` потрібне значення",
};

/// `Error::NotifySpawn`.
const NOTIFY_SPAWN: Msg = Msg {
    en: "cannot start the notifier beside this executable: {source}",
    uk: "не можна запустити нотифікатор поруч із цим файлом: {source}",
};

/// `Error::RepeatedOption`.
const REPEATED_OPTION: Msg = Msg {
    en: "option `{arg}` is given twice",
    uk: "параметр `{arg}` вказано двічі",
};

/// `Error::SearchCaNotSet` (AGL-043 F20).
const SEARCH_CA_NOT_SET: Msg = Msg {
    en: "agl search login needs `search.ca` with url, root and provisioner in {user_path}; `{key}` is not set:\n\nsearch.ca = {{ url = \"https://ca.example\"; root = \"/path/to/root.crt\"; provisioner = \"NAME\"; }};",
    uk: "agl search login потребує `search.ca` з url, root і provisioner у {user_path}; `{key}` не задано:\n\nsearch.ca = {{ url = \"https://ca.example\"; root = \"/path/to/root.crt\"; provisioner = \"NAME\"; }};",
};

/// `Error::SearchLogin` (AGL-043 F20).
const SEARCH_LOGIN: Msg = Msg {
    en: "agl search login failed: {detail}",
    uk: "agl search login не вдався: {detail}",
};

/// `Error::Terminal`.
const TERMINAL: Msg = Msg {
    en: "cannot switch the terminal to raw mode: {source}",
    uk: "не вдалося перемкнути термінал у raw-режим: {source}",
};

/// `Error::UnknownOption`.
const UNKNOWN_OPTION: Msg = Msg {
    en: "unknown option `{arg}`",
    uk: "невідомий параметр `{arg}`",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_errors_point_to_help() {
        let error = Error::UnknownOption("--url".into());
        assert_eq!(
            error.message(Lang::En),
            "unknown option `--url`\nRun `agl --help` for usage."
        );
        assert_eq!(
            error.message(Lang::Uk),
            "невідомий параметр `--url`\nДив. `agl --help`."
        );
        assert!(
            !Error::from(CoreError::from(crate::function::error::Error::NotFound {
                name: "x".into(),
                paths: vec!["/p".into()]
            }))
            .message(Lang::En)
            .contains("--help")
        );
        // The core's usage errors point to the help once, and exit 2 (D76).
        let resume = Error::from(CoreError::PlaneWithResume);
        assert_eq!(resume.message(Lang::En).matches("agl --help").count(), 1);
        assert_eq!(resume.exit_code(), 2);
    }
}
