//! Errors reported to the user. [`crate::i18n`] renders their messages.

use std::io;
use std::path::PathBuf;

use crate::settings::ServerKind;
use crate::style::StyleError;

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
    /// `--plane` with `--resume`: the session keeps its plane (AGL-077).
    PlaneWithResume,
    /// `--think` names a level outside the four (AGL-013).
    BadReasoningLevel { value: String },
    /// Neither `var` nor `HOME` is an absolute path.
    NoHome { var: &'static str },
    /// The current directory cannot be read.
    CurrentDir(io::Error),
    /// A settings file cannot be read.
    Read { path: PathBuf, source: io::Error },
    /// A settings file has a key outside `allowed` (D60).
    UnknownKey {
        path: PathBuf,
        key: String,
        allowed: Vec<&'static str>,
    },
    /// A required key is missing.
    MissingKey { path: PathBuf, key: String },
    /// A settings value has the wrong type.
    WrongType {
        path: PathBuf,
        key: String,
        expected: Expected,
    },
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
    /// The kind of a Function's model is not `llama` or `strata` (AGL-030, D1;
    /// AGL-036, D1).
    UnknownServerKind { path: PathBuf, value: String },
    /// `repl.thinking`, `repl.tool` or `repl.result` is not `off`,
    /// `flexible` or `full` (AGL-092, D2; AGL-095, D1).
    UnknownDetailLevel {
        path: PathBuf,
        key: String,
        value: String,
    },
    /// `search.url` is not an `https://` URL (AGL-019, D3).
    UnsupportedSearchUrl { path: PathBuf, url: String },
    /// `agl search login` without a complete `search.ca` in the user file
    /// (AGL-043 F20); `key` is the first one missing.
    SearchCaNotSet { user_path: PathBuf, key: String },
    /// A step of `agl search login` failed (AGL-043 F20).
    SearchLogin { detail: String },
    /// A file path of `search` is not absolute (AGL-019, D3).
    SearchPathNotAbsolute {
        path: PathBuf,
        key: String,
        value: String,
    },
    /// A credential file of `search` the policy refuses (AGL-019, D4). Its
    /// own type, wrapped (AGL-121, D66).
    Search(crate::search::Error),
    /// A format or Function name that cannot name a file.
    InvalidName(String),
    /// A template or string of a format is unusable (D81). Its own type,
    /// wrapped (AGL-121, D66).
    Format(crate::format::Error),
    /// A Function not found, built in, existing already, or unusable at a
    /// key (D21; AGL-102). Its own type, wrapped (AGL-121, D66).
    Function(crate::function::error::Error),
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
    /// A session not found, its name, its file's lines or its writing
    /// (D107; AGL-062). Its own type, wrapped (AGL-121, D66).
    Session(crate::session::error::Error),
    /// The machine cannot continue a session's turn. Its own type, wrapped
    /// (AGL-121, D66).
    Machine(crate::machine::error::Error),
    /// The terminal cannot be switched to raw mode.
    Terminal { source: io::Error },
    /// Ctrl-C stopped a generation (D111).
    Interrupted,
    /// The server generated text the grammar does not allow.
    OutsideGrammar(String),
    /// `--attach` without `-p` (AGL-014, D21).
    AttachWithoutPrompt,
    /// A weights file of the run is not there (AGL-043 F5): `key` is the
    /// setting that names it (`model.weights`, `model.mmproj`,
    /// `model.draft`, `whisper.model`) and `path` the file looked for.
    WeightsFileMissing { key: String, path: PathBuf },
    /// `model.draft` is set but `spec_type` is not (AGL-025, D4).
    MissingSpecType { draft: String },
    /// `agl server start` with a `url` whose host is not local (D148).
    NotLocalServer { url: String },
    /// The file of `private_ca` cannot be used (AGL-058, D12).
    ServerCa { path: PathBuf, detail: String },
    /// The server's certificate is not trusted (AGL-058, D12); `key` is the
    /// setting that names a private CA for that server (AGL-121, D4).
    ServerCertificate { url: String, detail: String, key: &'static str },
    /// `server.llama.external`, and nothing answers there (AGL-052, D4).
    OutsideServerDown { url: String },
    /// `server.llama.external`, and it serves another model (AGL-052, D5).
    OutsideModelDiffers { url: String, expected: String, running: String },
    /// `agl server command` of a Function on another kind of server (AGL-052, D12).
    ServerCommandNotLlama { function: String },
    /// The Windows engine in WSL could not start or run (AGL-052, D9, D10;
    /// AGL-056, D8). Its own type, wrapped (AGL-121, D66).
    Wsl(crate::wsl::Error),
    /// No engine binary in `PATH` (D147, AGL-024, D5).
    EngineNotFound { engine: String },
    /// The engine agl launched cannot be run (AGL-024, D2).
    EngineSpawn { engine: String, source: io::Error },
    /// `systemd-run` or `systemctl` failed.
    Systemd { detail: String },
    /// `agl notify` lost or could not reach the session bus (AGL-074, D2).
    Notify { detail: String },
    /// A plane's name, store, files or directories (AGL-077). Its own
    /// type, wrapped (AGL-121, D66).
    Plane(crate::plane::error::Error),
    /// What the daemon refused, in its words (AGL-076).
    Said { text: String },
    /// `agl daemon` has no runtime directory for its socket (AGL-076, D7).
    DaemonNoRuntime,
    /// Another daemon answers on the socket.
    DaemonRunning { path: PathBuf },
    /// The daemon's socket or a connection failed.
    Daemon { detail: String },
    /// No session of that id is open in the daemon.
    DaemonNoSession { id: String },
    /// `transcribe` for a directory with no session open in the daemon
    /// (AGL-076, D15).
    DaemonNoTranscriber { dir: PathBuf },
    /// `transcribe`'s audio is not base64.
    DaemonBadAudio,
    /// The thread of a run ended before it answered: a fault of agl's
    /// (AGL-075, D7).
    PlaneGone,
    /// The server stopped before it was ready (D149; AGL-024, D2).
    ServerFailed { owner: ServerOwner },
    /// The server was not ready in time (D149; AGL-024, D2).
    ServerTimeout { seconds: u64, owner: ServerOwner },
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
    /// The Strata engine's own error (AGL-089, D20, D9, D13). Its own type,
    /// wrapped (AGL-121, D66).
    Strata(crate::strata::Error),
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
    /// The whisper server was not set or could not transcribe (AGL-014,
    /// D22). Its own type, wrapped (AGL-121, D66).
    Whisper(crate::whisper::Error),
    /// The prompt names a medium but the server has no multimodal
    /// projector (AGL-014).
    NoMultimodal,
    /// A medium: changed, of no kind agl takes, or ffmpeg failing on it
    /// (AGL-014). Its own type, wrapped (AGL-121, D66).
    Media(crate::media::Error),
    /// The draft engine (AGL-122) cannot be run or failed: the draft is
    /// dropped, the final pass is untouched.
    Draft { detail: String },
    /// A weights file was missing (AGL-039, D24c; AGL-050, D11). Its own
    /// type, wrapped (AGL-121, D66).
    Weights(crate::weights::Error),
    /// A weights file failed to download (AGL-050, D17). Its own type,
    /// wrapped (AGL-121, D66).
    Download(crate::download::Error),
    /// Nix evaluation: the directory, the evaluator, a failed evaluation, a
    /// file that is not a function (AGL-039, D6-D13, D38). Its own type,
    /// wrapped (AGL-121, D66).
    Nix(crate::nix::Error),
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
    /// `agl memory` or `/memory`: no entry `name` (AGL-060).
    MemoryNoEntry { name: String },
    /// A removal with no terminal to ask on and no `--yes` (AGL-060, D3, D7).
    MemoryRemoveNeedsYes { name: String },
    /// A removal that could not be done (AGL-060).
    MemoryRemoveFailed { reason: String },
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
    /// `agl notify` cannot start the `agl-notify` beside it (AGL-121, D23).
    NotifySpawn { source: io::Error },
    /// A task-spec id does not resolve in the workspace repository.
    CommitNotFound { id: String, repo: PathBuf },
    /// An artifact file cannot be removed.
    Remove { path: PathBuf, source: io::Error },
}

impl From<agentlibre_client::Error> for Error {
    /// A client's error as agl's: the daemon, its words, its end
    /// (AGL-121, D29). The clients' crate has no i18n; agl's words are here.
    fn from(source: agentlibre_client::Error) -> Error {
        match source {
            agentlibre_client::Error::NoRuntime => Error::DaemonNoRuntime,
            agentlibre_client::Error::Daemon { detail } => Error::Daemon { detail },
            agentlibre_client::Error::Said { text } => Error::Said { text },
            agentlibre_client::Error::Gone => Error::PlaneGone,
        }
    }
}

impl From<crate::plane::error::Error> for Error {
    /// A plane's own error as agl's (AGL-121, D66).
    fn from(source: crate::plane::error::Error) -> Error {
        Error::Plane(source)
    }
}

impl From<crate::media::Error> for Error {
    /// A medium's own error as agl's (AGL-121, D66).
    fn from(source: crate::media::Error) -> Error {
        Error::Media(source)
    }
}

impl From<crate::function::error::Error> for Error {
    /// A Function's own error as agl's (AGL-121, D66).
    fn from(source: crate::function::error::Error) -> Error {
        Error::Function(source)
    }
}

impl From<crate::session::error::Error> for Error {
    /// A session's own error as agl's (AGL-121, D66).
    fn from(source: crate::session::error::Error) -> Error {
        Error::Session(source)
    }
}

impl From<crate::machine::error::Error> for Error {
    /// The machine's own error as agl's (AGL-121, D66).
    fn from(source: crate::machine::error::Error) -> Error {
        Error::Machine(source)
    }
}

impl From<crate::wsl::Error> for Error {
    /// The Windows engine's own error as agl's (AGL-121, D66).
    fn from(source: crate::wsl::Error) -> Error {
        Error::Wsl(source)
    }
}

impl From<crate::download::Error> for Error {
    /// A download's own error as agl's (AGL-121, D66).
    fn from(source: crate::download::Error) -> Error {
        Error::Download(source)
    }
}

impl From<crate::weights::Error> for Error {
    /// A weights lookup's own error as agl's (AGL-121, D66).
    fn from(source: crate::weights::Error) -> Error {
        Error::Weights(source)
    }
}

impl From<crate::whisper::Error> for Error {
    /// The whisper server's own error as agl's (AGL-121, D66).
    fn from(source: crate::whisper::Error) -> Error {
        Error::Whisper(source)
    }
}

impl From<crate::search::Error> for Error {
    /// The search credential policy's own error as agl's (AGL-121, D66).
    fn from(source: crate::search::Error) -> Error {
        Error::Search(source)
    }
}

impl From<crate::format::Error> for Error {
    /// A format template's own error as agl's (AGL-121, D66).
    fn from(source: crate::format::Error) -> Error {
        Error::Format(source)
    }
}

impl From<crate::strata::Error> for Error {
    /// A Strata engine's own error as agl's (AGL-121, D66).
    fn from(source: crate::strata::Error) -> Error {
        Error::Strata(source)
    }
}

impl From<crate::nix::Error> for Error {
    /// A Nix evaluation's own error as agl's (AGL-121, D66).
    fn from(source: crate::nix::Error) -> Error {
        Error::Nix(source)
    }
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
    /// A list of sets (AGL-089).
    Tables,
}

/// Who launched a server that did not get ready (AGL-043 F5): the
/// systemd unit, whose output is in its journal, or agl's own child,
/// whose output is in its log (AGL-024, D2).
#[derive(Debug)]
pub enum ServerOwner {
    Unit(String),
    Child(PathBuf),
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
                | Error::PlaneWithResume
                | Error::BadReasoningLevel { .. }
                | Error::AttachWithoutPrompt
        )
    }

    /// Process exit status for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Interrupted => crate::stop::EXIT_CODE,
            _ if self.is_usage() => 2,
            _ => 1,
        }
    }
}

/// The English message, for places that keep an error as text (`forge` list
/// marks the files it cannot parse with it): the one text of `crate::i18n`
/// (AGL-058, D22).
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(crate::i18n::Lang::En))
    }
}
