//! Errors reported to the user. [`crate::i18n`] renders their messages.

use std::io;
use std::path::PathBuf;

use crate::grammar::pattern::PatternError;
use crate::settings::ServerKind;
use crate::theme::StyleError;

/// An error that stops `agl`.
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
    /// `--think` names a level outside the four (AGL-013).
    BadReasoningLevel { value: String },
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
    /// The `url` of `[server.llama]` or `[server.strata]` is not an
    /// `http://` URL (D52; AGL-036, D2).
    UnsupportedUrl { path: PathBuf, url: String },
    /// The `kind` of `[server]` or of a function file's `[server]` is not
    /// `llama` or `strata` (AGL-030, D1; AGL-036, D1).
    UnknownServerKind { path: PathBuf, value: String },
    /// `[search] url` is not an `https://` URL (AGL-019, D3).
    UnsupportedSearchUrl { path: PathBuf, url: String },
    /// A file path of `[search]` is not absolute (AGL-019, D3).
    SearchPathNotAbsolute {
        path: PathBuf,
        key: String,
        value: String,
    },
    /// A credential file of `[search]` that the policy refuses
    /// (AGL-019, D4).
    SearchCredential {
        file: PathBuf,
        problem: CredentialProblem,
    },
    /// No `[server.llama] url` in the project's file nor in the user file
    /// (D59, AGL-015; AGL-036, D2, D6).
    MissingServerUrl {
        user_path: PathBuf,
        project_path: PathBuf,
    },
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
    /// Neither the looked-in `paths` nor a built-in Function is called
    /// `name`.
    FunctionNotFound { name: String, paths: Vec<PathBuf> },
    /// A Function is unusable; `key` locates the problem (D21, D42, D72,
    /// D78, D82-D93).
    Function {
        path: PathBuf,
        key: String,
        problem: FunctionProblem,
    },
    /// The server of `kind` cannot be reached (D97; AGL-038, D1).
    ServerUnreachable {
        kind: ServerKind,
        url: String,
        detail: String,
    },
    /// The server of `kind` answered with an error status.
    ServerStatus {
        kind: ServerKind,
        url: String,
        status: u16,
        message: String,
    },
    /// The server of `kind` answered something agl does not understand.
    ServerReply {
        kind: ServerKind,
        url: String,
        detail: String,
    },
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
    /// `--attach` without `-p` (AGL-014, D21).
    AttachWithoutPrompt,
    /// `agl server start` without `[server] model` (D148).
    MissingModel {
        user_path: PathBuf,
        project_path: PathBuf,
        models: PathBuf,
    },
    /// `[server] model` names no file in the models directory.
    ModelNotFound { path: PathBuf },
    /// `[server] draft` names no file in the models directory (AGL-025, D2).
    DraftNotFound { path: PathBuf },
    /// `[server] draft` is set but `spec_type` is not (AGL-025, D4).
    MissingSpecType { draft: String },
    /// `agl server start` with a `url` whose host is not local (D148).
    NotLocalServer { url: String },
    /// No engine binary in `PATH` (D147, AGL-024, D5).
    EngineNotFound { engine: String },
    /// The engine agl launched cannot be run (AGL-024, D2).
    EngineSpawn { engine: String, source: io::Error },
    /// `systemd-run` or `systemctl` failed.
    Systemd { detail: String },
    /// The unit stopped before the server was ready (D149).
    ServerFailed { unit: String },
    /// The server was not ready in time (D149).
    ServerTimeout { seconds: u64, unit: String },
    /// The engine agl launched exited before it was ready (AGL-024, D2);
    /// its output is in `log`.
    EngineDied { log: PathBuf },
    /// The engine agl launched was not ready in time (AGL-024, D2); its
    /// output is in `log`.
    EngineTimeout { seconds: u64, log: PathBuf },
    /// A server command or the autostart outside a registered project
    /// (AGL-015, D14).
    ProjectNotRegistered { project: PathBuf },
    /// The port of the project's `url` is held by something that is not
    /// its unit; `units` are the server units that run (D7).
    PortBusy { url: String, units: Vec<String> },
    /// The server of another project runs and nobody can be asked: `-p`
    /// and the stdin session without `--yes` (D13).
    ServerRunning { units: Vec<String> },
    /// The human answered no to "start one more?" (D9).
    StartDeclined,
    /// The human answered no to the restart of D2 (AGL-033).
    RestartDeclined,
    /// The human answered no to the stop of the server of the other kind
    /// (AGL-036, D10).
    SwitchDeclined,
    /// The `[server]` of the strata kind names no `strata_config` (AGL-033,
    /// D2).
    MissingStrataConfig {
        user_path: PathBuf,
        project_path: PathBuf,
    },
    /// The Strata config JSON the `strata_config` names cannot be read or
    /// parsed (AGL-033, D2).
    StrataConfig { path: PathBuf, detail: String },
    /// The Strata config names no `cwd`, the Strata checkout (AGL-038, D3,
    /// D7).
    StrataNoCwd { path: PathBuf },
    /// The `.venv/bin/python` in the `cwd` of the Strata config is missing
    /// (AGL-033, D2; AGL-038, D3).
    StrataPython { path: PathBuf },
    /// The `serve/server.py` in the `cwd` of the Strata config is missing
    /// (AGL-033, D2; AGL-038, D3).
    StrataServerPy { path: PathBuf },
    /// An audio needs whisper and `[whisper]` lacks `url` or `model`
    /// (AGL-014, D22).
    WhisperNotSet {
        user_path: PathBuf,
        project_path: PathBuf,
    },
    /// No `whisper-server` in `PATH`.
    WhisperServerNotFound,
    /// The whisper server answered an error or nothing.
    Transcribe { url: String, detail: String },
    /// The prompt names a medium but the server has no multimodal
    /// projector (AGL-014).
    NoMultimodal,
    /// A medium the session keeps is not what its name says (AGL-014, D7).
    MediaChanged { path: PathBuf },
    /// A file to attach that is not an image, a video or an audio.
    MediaKind { path: PathBuf },
    /// `ffmpeg` or `ffprobe` cannot be run or failed.
    Ffmpeg { detail: String },
    /// `AGENTLIBRE_TOOLS` is not set to an absolute directory: agl runs
    /// outside its package's wrapper and the dev shell (AGL-040).
    ToolsDirMissing,
    /// A helper program is not in the `AGENTLIBRE_TOOLS` directory.
    ToolMissing { name: String, dir: PathBuf },
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
    /// A forge file or directory cannot be written.
    Write { path: PathBuf, source: io::Error },
    /// The `format` of a workspace marker is not 1.
    MarkerFormat { path: PathBuf, format: i64 },
    /// A workspace marker lists a value that is not a `sha256:<hex>`
    /// identity.
    MarkerIdentity { path: PathBuf, value: String },
    /// The current identity is in the markers of two stores: one was
    /// copied by hand.
    WorkspaceAmbiguous { dirs: Vec<String> },
    /// `forge rehome` without a store off a TTY.
    RehomeNoStore { stores: Vec<String> },
    /// No store is named `name`.
    StoreNotFound { name: String, stores: Vec<String> },
    /// An artifact file has no `---` front-matter delimiters.
    ArtifactFrontMatter { path: PathBuf },
    /// An artifact file carries a schema the forge does not parse.
    ArtifactUnknownSchema { path: PathBuf, schema: String },
    /// The `[<type>]` table of an artifact is missing.
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
    /// The `anchor` of a task-spec is not an ancestor of its `last`.
    BrokenRange { anchor: String, last: String },
    /// An artifact file cannot be removed.
    Remove { path: PathBuf, source: io::Error },
}

/// What the credential policy of `[search]` finds wrong with a file
/// (AGL-019, D4).
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialProblem {
    /// The file cannot be inspected or read; `detail` is the system's.
    Unreadable(String),
    /// Not a regular file (a link to one counts as not).
    NotRegular,
    /// Not owned by the user who runs `agl`.
    NotOwn,
    /// Readable or writable by the group or others.
    OthersAccess,
    /// More than one hard link.
    OtherLinks,
    Empty,
    /// Over 1 MiB.
    TooLarge,
    /// Not a PEM of the type the key needs (`expected`).
    WrongType { expected: &'static str },
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
    /// The `[server]` table of a Function has no `model`, the key of the
    /// llama kind (AGL-033, D1).
    ServerModelMissing,
    /// The `[server]` table of a Function has no `strata_config`, the key
    /// of the strata kind (AGL-033, D1).
    ServerStrataConfigMissing,
    /// A key of the `[server]` table of a Function that does not apply to
    /// the kind of the project's `[server]` (AGL-033, D1).
    ServerKeyOfOtherKind(String),
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
                | Error::BadReasoningLevel { .. }
                | Error::AttachWithoutPrompt
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

/// A plain English rendering, for places that keep an error as text
/// (`forge` list marks the files it cannot parse with it).
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let write = |f: &mut std::fmt::Formatter<'_>, text: String| f.write_str(&text);
        write(
            f,
            match self {
                Error::InvalidUnicode => "arguments must be valid UTF-8".into(),
                Error::UnknownOption(arg) => format!("unknown option `{arg}`"),
                Error::MissingValue(arg) => format!("option `{arg}` needs a value"),
                Error::RepeatedOption(arg) => format!("option `{arg}` is given twice"),
                Error::FunctionWithResume => {
                    "`--function` cannot go with `--resume`".into()
                }
                Error::BadReasoningLevel { value } => format!(
                    "invalid reasoning level `{value}` for `--think`: \
                     the levels are off, low, medium and xhigh"
                ),
                Error::NoHome { var } => format!("HOME is not set and {var} is not absolute"),
                Error::CurrentDir(source) => format!("cannot read the current directory: {source}"),
                Error::Read { path, source } => format!("cannot read {}: {source}", path.display()),
                Error::Parse { path, detail } => {
                    format!("invalid TOML in {}: {detail}", path.display())
                }
                Error::UnknownKey { path, key, .. } => {
                    format!("unknown key `{key}` in {}", path.display())
                }
                Error::MissingKey { path, key } => {
                    format!("`{key}` is missing in {}", path.display())
                }
                Error::WrongType { path, key, .. } => {
                    format!("`{key}` in {} has the wrong type", path.display())
                }
                Error::InvalidStyle { path, key, .. } => {
                    format!("invalid style for `{key}` in {}", path.display())
                }
                Error::NotPositive { path, key } => {
                    format!("`{key}` in {} must be at least 1", path.display())
                }
                Error::UnsupportedLanguage { path, value } => {
                    format!("unsupported language `{value}` in {}", path.display())
                }
                Error::UnsupportedUrl { path, url } => {
                    format!("unsupported server address `{url}` in {}", path.display())
                }
                Error::UnknownServerKind { path, value } => {
                    format!("`kind` in {} is not `llama` or `strata`: `{value}`", path.display())
                }
                Error::UnsupportedSearchUrl { path, url } => {
                    format!("unsupported search address `{url}` in {}", path.display())
                }
                Error::SearchPathNotAbsolute { path, key, .. } => {
                    format!("`{key}` in {} must be an absolute path", path.display())
                }
                Error::SearchCredential { file, .. } => {
                    format!("the search credential {} is refused", file.display())
                }
                Error::MissingServerUrl { .. } => "the llama-server address is not set".into(),
                Error::InvalidName(name) => format!("invalid name `{name}`"),
                Error::FormatNotFound { name, .. } => format!("model format `{name}` not found"),
                Error::Template { path, key, .. } => {
                    format!("a problem with `{key}` in {}", path.display())
                }
                Error::FunctionNotFound { name, .. } => {
                    format!("Function `{name}` not found")
                }
                Error::Function { path, key, .. } => {
                    format!("a problem with `{key}` in {}", path.display())
                }
                Error::ServerUnreachable { kind, url, .. } => {
                    format!("cannot reach {} at {url}", kind.server())
                }
                Error::ServerStatus { kind, url, status, .. } => {
                    format!("{} at {url} answered {status}", kind.server())
                }
                Error::ServerReply { kind, url, .. } => {
                    format!("unexpected reply from {} at {url}", kind.server())
                }
                Error::SessionNotFound { id, dir } => {
                    format!("no session `{id}` in {}", dir.display())
                }
                Error::SessionAmbiguous { id, count } => {
                    format!("{count} sessions start with `{id}`")
                }
                Error::SessionLine { path, line, detail } => {
                    format!("line {line} of {}: {detail}", path.display())
                }
                Error::SessionWrite { path, source } => {
                    format!("cannot write {}: {source}", path.display())
                }
                Error::SessionState { state, function } => {
                    format!("the session state `{state}` is not in Function `{function}`")
                }
                Error::Terminal { source } => format!("cannot switch the terminal to raw mode: {source}"),
                Error::Interrupted => "interrupted".into(),
                Error::OutsideGrammar(text) => {
                    format!("generated text outside the grammar: {text:?}")
                }
                Error::MissingModel { .. } => "the model is not set".into(),
                Error::ModelNotFound { path } => format!("no model file {}", path.display()),
                Error::DraftNotFound { path } => {
                    format!("no draft model file {}", path.display())
                }
                Error::MissingSpecType { draft } => {
                    format!("the draft {draft} is set but the spec_type is not: set the spec_type too")
                }
                Error::NotLocalServer { url } => {
                    format!("the server address is not local: {url}")
                }
                Error::EngineNotFound { engine } => format!("{engine} is not in PATH"),
                Error::EngineSpawn { engine, source } => {
                    format!("cannot run {engine}: {source}")
                }
                Error::Systemd { detail } => format!("systemd failed: {detail}"),
                Error::ServerFailed { .. } => "the server stopped before it was ready".into(),
                Error::EngineDied { log } => format!(
                    "the engine stopped before it was ready; see {}",
                    log.display()
                ),
                Error::EngineTimeout { seconds, log } => format!(
                    "the engine was not ready after {seconds} s; see {}",
                    log.display()
                ),
                Error::ProjectNotRegistered { project } => {
                    format!("{} is not a registered project", project.display())
                }
                Error::PortBusy { url, .. } => format!("the port of {url} is busy"),
                Error::ServerRunning { units } => {
                    format!("a server is running: {}", units.join(", "))
                }
                Error::StartDeclined => "the server was not started".into(),
                Error::RestartDeclined => "the server was not restarted".into(),
                Error::SwitchDeclined => "the server of the other kind was not stopped".into(),
                Error::MissingStrataConfig { .. } => {
                    "the strata kind needs strata_config (the path of the Strata config JSON)"
                        .into()
                }
                Error::StrataConfig { path, detail } => {
                    format!("the Strata config {} cannot be read: {detail}", path.display())
                }
                Error::StrataNoCwd { path } => {
                    format!("the Strata config {} names no cwd", path.display())
                }
                Error::StrataPython { path } => {
                    format!(
                        "no venv python at {} (the .venv in the cwd of the Strata config)",
                        path.display()
                    )
                }
                Error::StrataServerPy { path } => {
                    format!(
                        "no serve/server.py at {} (in the cwd of the Strata config)",
                        path.display()
                    )
                }
                Error::ServerTimeout { seconds, .. } => {
                    format!("the server was not ready after {seconds} s")
                }
                Error::AttachWithoutPrompt => "--attach goes with -p".into(),
                Error::NoMultimodal => "the server has no multimodal projector".into(),
                Error::WhisperNotSet { .. } => "the whisper server is not set".into(),
                Error::WhisperServerNotFound => "whisper-server is not in PATH".into(),
                Error::Transcribe { url, detail } => {
                    format!("the whisper server at {url} could not transcribe: {detail}")
                }
                Error::MediaChanged { path } => {
                    format!("{} is not what its name says", path.display())
                }
                Error::MediaKind { path } => {
                    format!("{} is not an image, a video or an audio", path.display())
                }
                Error::Ffmpeg { detail } => format!("ffmpeg failed: {detail}"),
                Error::ToolsDirMissing => format!(
                    "{} is not set: run agl from its Nix package or `nix develop`",
                    crate::programs::TOOLS_VAR
                ),
                Error::ToolMissing { name, dir } => {
                    format!("{name} is not in {}", dir.display())
                }
                Error::GitSpawn { source } => format!("cannot run git: {source}"),
                Error::GitFailed { args, detail } => format!("git {args} failed: {detail}"),
                Error::NotInRepository { cwd } => {
                    format!("{} is not in a git repository", cwd.display())
                }
                Error::StoreNotRepository { dir } => {
                    format!(
                        "the store directory is not a git repository: {}",
                        dir.display()
                    )
                }
                Error::Write { path, source } => {
                    format!("cannot write {}: {source}", path.display())
                }
                Error::MarkerFormat { path, format } => {
                    format!("{}: the marker format is {format}, expected 1", path.display())
                }
                Error::MarkerIdentity { path, value } => {
                    format!("{} lists {value}, which is not a sha256 identity", path.display())
                }
                Error::WorkspaceAmbiguous { dirs } => {
                    format!("the workspace identity is in the markers of {}", dirs.join(", "))
                }
                Error::RehomeNoStore { .. } => "a store cannot be chosen without a terminal".into(),
                Error::StoreNotFound { name, .. } => format!("no store is named {name}"),
                Error::ArtifactFrontMatter { path } => {
                    format!("{} has no `---` front-matter", path.display())
                }
                Error::ArtifactUnknownSchema { path, schema } => {
                    format!("{} has schema {schema}, expected forge/v1", path.display())
                }
                Error::ArtifactTypeTableMissing { path, type_name } => {
                    format!("{} has no [{type_name}] table", path.display())
                }
                Error::ArtifactKey { path, key } => {
                    format!("{} has an unknown key `{key}`", path.display())
                }
                Error::ArtifactDate { path, key, value } => {
                    format!("{} has {key} = {value}, not a YYYY-MM-DD date", path.display())
                }
                Error::ArtifactNotFound { name, dir } => {
                    format!("no artifact is named {name} in {}", dir.display())
                }
                Error::BadState { state } => {
                    format!(
                        "{state} is not a task-spec state (open, in-progress, done, \
                         backlog, canceled)"
                    )
                }
                Error::BadTransition { from, to } => {
                    format!("a task-spec cannot go from {from} to {to}")
                }
                Error::TerminalState { name, from } => {
                    format!(
                        "{name} is {from}; {from} is a terminal state — to continue the \
                         work, make a new spec with `supersedes = [{name}]`"
                    )
                }
                Error::NewType { type_name } => format!("forge new does not scaffold {type_name}"),
                Error::NameTaken { name, dir } => {
                    format!("an artifact named {name} is already in {}", dir.display())
                }
                Error::NotTaskSpec { name, type_name } => {
                    format!("{name} is a {type_name}, not a task-spec")
                }
                Error::RehomeInUse { current, target } => {
                    format!(
                        "the identity is already in the store {current}; it cannot also be in {target}"
                    )
                }
                Error::CmdSpawn { source } => {
                    format!("the check command cannot be run: {source}")
                }
                Error::CommitNotFound { id, repo } => {
                    format!("{id} is not a commit in {}", repo.display())
                }
                Error::BrokenRange { anchor, last } => {
                    format!("{anchor} is not an ancestor of {last}")
                }
                Error::Remove { path, source } => {
                    format!("cannot remove {}: {source}", path.display())
                }
            },
        )
    }
}
