//! Errors reported to the user. [`crate::i18n`] renders their messages.

use std::io;
use std::path::PathBuf;

use crate::grammar::pattern::PatternError;
use crate::theme::StyleError;

/// An error that stops `agentLIBRE`.
#[derive(Debug)]
pub enum Error {
    /// An argument is not valid UTF-8.
    InvalidUnicode,
    /// An argument is not a known option.
    UnknownOption(String),
    /// An option is the last argument.
    MissingValue(String),
    /// An option is given twice.
    RepeatedOption(String),
    /// `--function` with `--resume`: the session names its Function (D106).
    FunctionWithResume,
    /// Neither `var` nor `HOME` is an absolute path.
    NoHome { var: &'static str },
    /// The current directory cannot be read.
    CurrentDir(io::Error),
    /// A settings file cannot be read.
    Read { path: PathBuf, source: io::Error },
    /// A settings file is not valid TOML; `detail` comes from the parser.
    Parse { path: PathBuf, detail: String },
    /// A settings file has a key outside `allowed` (D60).
    UnknownKey {
        path: PathBuf,
        key: String,
        allowed: &'static [&'static str],
    },
    /// A required key is missing.
    MissingKey { path: PathBuf, key: String },
    /// A settings value has the wrong TOML type.
    WrongType {
        path: PathBuf,
        key: String,
        expected: Expected,
    },
    /// A `[colors]` value is not a valid style (D121).
    InvalidStyle {
        path: PathBuf,
        key: String,
        value: String,
        problem: StyleError,
    },
    /// A `[repl]` limit below 1 (D130).
    NotPositive { path: PathBuf, key: String },
    /// `language` names a language without translations.
    UnsupportedLanguage { path: PathBuf, value: String },
    /// `[server] url` is not an `http://` URL (D52).
    UnsupportedUrl { path: PathBuf, url: String },
    /// A project file sets `[server]` (D57).
    ServerInProject { path: PathBuf, user_path: PathBuf },
    /// No `[server] url` in the user file (D59).
    MissingServerUrl { user_path: PathBuf },
    /// A format or Function name that cannot name a file.
    InvalidName(String),
    /// Neither `path` nor a built-in format is called `name`.
    FormatNotFound { name: String, path: PathBuf },
    /// A template or string of a format is unusable (D81).
    Template {
        path: PathBuf,
        key: String,
        problem: TemplateProblem,
    },
    /// Neither `path` nor a built-in Function is called `name`.
    FunctionNotFound { name: String, path: PathBuf },
    /// A Function is unusable; `key` locates the problem (D21, D42, D72,
    /// D78, D82-D93).
    Function {
        path: PathBuf,
        key: String,
        problem: FunctionProblem,
    },
    /// llama-server cannot be reached (D97).
    ServerUnreachable { url: String, detail: String },
    /// llama-server answered with an error status.
    ServerStatus {
        url: String,
        status: u16,
        message: String,
    },
    /// llama-server answered something agentLIBRE does not understand.
    ServerReply { url: String, detail: String },
    /// No session in `dir` has an id that is or starts with `id`.
    SessionNotFound { id: String, dir: PathBuf },
    /// More than one session id starts with `id` (D107).
    SessionAmbiguous { id: String, count: usize },
    /// Line `line` of a session file is not a valid event.
    SessionLine {
        path: PathBuf,
        line: usize,
        detail: String,
    },
    /// A session file cannot be written.
    SessionWrite { path: PathBuf, source: io::Error },
    /// The session continues a turn in a state the Function no longer has.
    SessionState { state: String, function: String },
    /// The terminal cannot be switched to raw mode.
    Terminal { source: io::Error },
    /// Ctrl-C stopped a generation (D111).
    Interrupted,
    /// The server generated text the grammar does not allow.
    OutsideGrammar(String),
    /// `agentLIBRE server start` without `[server] model` (D148).
    MissingModel { user_path: PathBuf, models: PathBuf },
    /// `[server] model` names no file in the models directory.
    ModelNotFound { path: PathBuf },
    /// `agentLIBRE server start` with a `url` whose host is not local (D148).
    NotLocalServer { url: String },
    /// No `llama-server` in `PATH` (D147).
    LlamaServerNotFound,
    /// `systemd-run` or `systemctl` failed.
    Systemd { detail: String },
    /// The unit stopped before llama-server was ready (D149).
    ServerFailed,
    /// llama-server was not ready in time (D149).
    ServerTimeout { seconds: u64 },
}

/// TOML type a settings key requires.
#[derive(Debug)]
pub enum Expected {
    String,
    Table,
    Bool,
    Integer,
    Number,
    Strings,
}

/// What is wrong with a format template (D81).
#[derive(Debug, PartialEq, Eq)]
pub enum TemplateProblem {
    /// A placeholder the template needs is absent.
    Missing(&'static str),
    /// A placeholder appears more than once.
    Repeated(&'static str),
    /// A placeholder that does not belong in this template.
    Unexpected(&'static str),
    /// `{value}` comes before `{name}`.
    ValueBeforeName,
    /// `begin` has no text before `{name}`, so answers cannot be told
    /// apart from calls.
    NothingBeforeName,
    /// `param` has no text after `{value}`, so values have no end.
    NothingAfterValue,
    /// A `think` string is empty.
    Empty,
}

/// What is wrong with a Function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionProblem {
    /// The value names a state that does not exist.
    UnknownState(String),
    /// A transition key is not a tool of the state, with or without an
    /// outcome, nor `else`.
    BadTransitionKey,
    /// No transition for a tool and outcome of the state (D21).
    MissingTransition { tool: String, outcome: &'static str },
    /// The state has neither tools nor `answer` (D87).
    NoActions,
    /// The state has `next` but no tools.
    NextWithoutTools,
    /// `on_limit` without `max_steps` or `max_visits`.
    OnLimitWithoutLimit,
    /// `on_limit` references lead back to this state (D83).
    OnLimitCycle,
    /// No transition leads to the state (D91).
    Unreachable,
    /// `think = true` with a format without thinking (D78).
    NoThinking { format: String },
    /// The pattern is not in the supported subset (D69, D75).
    Pattern(PatternError),
    /// The pattern may produce `end`, which ends values (D72).
    PatternMayEndValue { end: char },
    /// An empty `enum`.
    EmptyEnum,
    /// `min > max`, or `min < 1` for `timeout` (D93).
    BadRange,
    /// A constraint needs exactly one key (D88).
    ConstraintKeys,
    /// A limit below 1.
    NotPositive,
}

impl Error {
    /// Whether the error is in the command line rather than the setup.
    pub fn is_usage(&self) -> bool {
        matches!(
            self,
            Error::InvalidUnicode
                | Error::UnknownOption(_)
                | Error::MissingValue(_)
                | Error::RepeatedOption(_)
                | Error::FunctionWithResume
        )
    }

    /// Process exit status for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Interrupted => crate::interrupt::EXIT_CODE,
            _ if self.is_usage() => 2,
            _ => 1,
        }
    }
}
