//! What can go wrong with the engine and its server: finding and launching
//! it, its unit, its port, the human's answers (AGL-121, D66).

use std::io;
use std::path::PathBuf;

use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

/// The engine's own error, wrapped by the core's `Error::Engine`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// `agl server start` with a `url` whose host is not local (D148).
    NotLocalServer { url: String },
    /// `server.llama.external`, and nothing answers there (AGL-052, D4).
    OutsideServerDown { url: String },
    /// `server.llama.external`, and it serves another model (AGL-052, D5).
    OutsideModelDiffers { url: String, expected: String, running: String },
    /// `agl server command` of a Function on another kind of server (AGL-052, D12).
    ServerCommandNotLlama { function: String },
    /// No engine binary in `PATH` (D147, AGL-024, D5).
    EngineNotFound { engine: String },
    /// The engine agl launched cannot be run (AGL-024, D2).
    EngineSpawn { engine: String, source: io::Error },
    /// `systemd-run` or `systemctl` failed.
    Systemd { detail: String },
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
    /// A weights file of the run is not there (AGL-043 F5): `key` is the
    /// setting that names it (`model.weights`, `model.mmproj`,
    /// `model.draft`, `whisper.model`) and `path` the file looked for.
    WeightsFileMissing { key: String, path: PathBuf },
    /// `model.draft` is set but `spec_type` is not (AGL-025, D4).
    MissingSpecType { draft: String },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NotLocalServer { url } => tr!(lang, NOT_LOCAL_SERVER, url = url),
            Self::OutsideServerDown { url } => tr!(lang, OUTSIDE_SERVER_DOWN, url = url),
            Self::OutsideModelDiffers { url, expected, running } => tr!(
                lang,
                OUTSIDE_MODEL_DIFFERS,
                url = url,
                expected = expected,
                running = running
            ),
            Self::ServerCommandNotLlama { function } => {
                tr!(lang, SERVER_COMMAND_NOT_LLAMA, function = function)
            }
            Self::EngineNotFound { engine } => {
                tr!(lang, ENGINE_NOT_FOUND, engine = engine).to_string()
            }
            Self::EngineSpawn { engine, source } => {
                tr!(lang, ENGINE_SPAWN, engine = engine, source = source).to_string()
            }
            Self::Systemd { detail } => tr!(lang, SYSTEMD, detail = detail),
            Self::ServerFailed { owner } => match owner {
                ServerOwner::Unit(unit) => tr!(lang, SERVER_FAILED, unit = unit),
                ServerOwner::Child(log) => tr!(lang, ENGINE_DIED, log = log.display()),
            },
            Self::ServerTimeout { seconds, owner } => match owner {
                ServerOwner::Unit(unit) => {
                    tr!(lang, SERVER_TIMEOUT, seconds = seconds, unit = unit)
                }
                ServerOwner::Child(log) => {
                    tr!(lang, ENGINE_TIMEOUT, seconds = seconds, log = log.display())
                }
            },
            Self::ProjectNotRegistered { project } => {
                tr!(lang, PROJECT_NOT_REGISTERED, project = project.display())
            }
            Self::PortBusy { url, units } => {
                if units.is_empty() {
                    tr!(lang, PORT_BUSY, url = url)
                } else {
                    tr!(lang, PORT_BUSY_UNITS, url = url, units = units.join(", "))
                }
            }
            Self::ServerRunning { units } => {
                tr!(lang, SERVER_RUNNING, units = units.join(", "))
            }
            Self::StartDeclined => tr!(lang, START_DECLINED).to_string(),
            Self::RestartDeclined => tr!(lang, RESTART_DECLINED).to_string(),
            Self::SwitchDeclined => tr!(lang, SWITCH_DECLINED).to_string(),
            Self::WeightsFileMissing { key, path } => {
                tr!(lang, WEIGHTS_FILE_MISSING, path = path.display(), key = key)
            }
            Self::MissingSpecType { draft } => {
                tr!(lang, MISSING_SPEC_TYPE, draft = draft)
            }
        }
    }
}

/// Who launched a server that did not get ready (AGL-043 F5): the
/// systemd unit, whose output is in its journal, or agl's own child,
/// whose output is in its log (AGL-024, D2).
#[derive(Debug)]
pub enum ServerOwner {
    Unit(String),
    Child(PathBuf),
}

/// `Error::ServerFailed` of agl's child (AGL-024, D2).
const ENGINE_DIED: Msg = Msg {
    en: "the engine stopped before it was ready; see {log}",
    uk: "движок зупинився, не встигнувши стати готовим; див. {log}",
};

/// `Error::EngineNotFound` (AGL-024, D5).
const ENGINE_NOT_FOUND: Msg = Msg {
    en: "{engine} is not in PATH",
    uk: "{engine} немає в PATH",
};

/// `Error::EngineSpawn` (AGL-024, D2).
const ENGINE_SPAWN: Msg = Msg {
    en: "cannot run {engine}: {source}",
    uk: "не вдалося запустити {engine}: {source}",
};

/// `Error::ServerTimeout` of agl's child (AGL-024, D2).
const ENGINE_TIMEOUT: Msg = Msg {
    en: "the engine was not ready after {seconds} s; see {log}",
    uk: "движок не став готовим за {seconds} с; див. {log}",
};

/// `Error::MissingSpecType`.
const MISSING_SPEC_TYPE: Msg = Msg {
    en: "the draft {draft} is set but the spec_type is not: set the spec_type too",
    uk: "draft {draft} задано, але spec_type ні: задано spec_type теж",
};

/// `Error::NotLocalServer`.
const NOT_LOCAL_SERVER: Msg = Msg {
    en: "agl starts the server only on 127.0.0.1 or localhost, not at {url}",
    uk: "agl запускає сервер лише на 127.0.0.1 або localhost, не за {url}",
};

/// `Error::OutsideModelDiffers`.
const OUTSIDE_MODEL_DIFFERS: Msg = Msg {
    en: "the server at {url} serves {running}, and this Function needs {expected}: agl does not restart a server outside it",
    uk: "сервер за {url} обслуговує {running}, а цій функції потрібна {expected}: agl не перезапускає сервер поза собою",
};

/// `Error::OutsideServerDown`.
const OUTSIDE_SERVER_DOWN: Msg = Msg {
    en: "the server outside agl does not answer at {url}: start it there (`agl server command` prints the line), or check the address",
    uk: "сервер поза agl не відповідає за {url}: запусти його там (рядок друкує `agl server command`) або перевір адресу",
};

/// `Error::PortBusy` without a running unit (D7).
const PORT_BUSY: Msg = Msg {
    en: "the address {url} is already in use by something that is not a unit of agl; free it or set another `server.llama.url` (or `server.strata.url`) in the project's settings.nix",
    uk: "адреса {url} уже зайнята чимось, що не є unit-ом agl; звільни її або задай інший `server.llama.url` (чи `server.strata.url`) у settings.nix проєкту",
};

/// `Error::PortBusy` with running units (D7).
const PORT_BUSY_UNITS: Msg = Msg {
    en: "the address {url} is already in use; the server units that run: {units}. Stop the one that holds it (agl server stop) or set another `server.llama.url` (or `server.strata.url`) in the project's settings.nix",
    uk: "адреса {url} уже зайнята; працюють unit-и серверів: {units}. Зупини той, що її тримає (agl server stop), або задай інший `server.llama.url` (чи `server.strata.url`) у settings.nix проєкту",
};

/// `Error::ProjectNotRegistered` (AGL-015, D14).
const PROJECT_NOT_REGISTERED: Msg = Msg {
    en: "{project} is not a registered project, so it has no server of its own; run `agl init` there first",
    uk: "{project} не є зареєстрованим проєктом, тож не має власного сервера; спершу виконай там `agl init`",
};

/// `Error::RestartDeclined` (AGL-033, D2).
const RESTART_DECLINED: Msg = Msg {
    en: "the server was not restarted",
    uk: "сервер не перезапущено",
};

/// `Error::ServerCommandNotLlama`.
const SERVER_COMMAND_NOT_LLAMA: Msg = Msg {
    en: "the Function {function} does not run on llama-server, so there is no line to print",
    uk: "функція {function} працює не на llama-server, тож рядка для неї немає",
};

/// `Error::ServerFailed` of a unit.
const SERVER_FAILED: Msg = Msg {
    en: "the server stopped before it was ready; see journalctl --user -u {unit}",
    uk: "сервер зупинився, не встигнувши стати готовим; див. journalctl --user -u {unit}",
};

/// `Error::ServerRunning` (D13).
const SERVER_RUNNING: Msg = Msg {
    en: "a server of another project is running ({units}); to start one more, use --yes",
    uk: "працює сервер іншого проєкту ({units}); щоб запустити ще один, додай --yes",
};

/// `Error::ServerTimeout` of a unit.
const SERVER_TIMEOUT: Msg = Msg {
    en: "the server was not ready after {seconds} s; it keeps loading in the background (agl server status, journalctl --user -u {unit})",
    uk: "сервер не став готовим за {seconds} с; він вантажиться далі у фоні (agl server status, journalctl --user -u {unit})",
};

/// `Error::StartDeclined` (D9).
const START_DECLINED: Msg = Msg {
    en: "the server was not started",
    uk: "сервер не запущено",
};

/// `Error::SwitchDeclined` (AGL-036, D10).
const SWITCH_DECLINED: Msg = Msg {
    en: "the server of the other kind was not stopped, so the new one was not started",
    uk: "сервер іншого виду не зупинено, тож новий не запущено",
};

/// `Error::Systemd`.
const SYSTEMD: Msg = Msg {
    en: "systemd failed: {detail}",
    uk: "помилка systemd: {detail}",
};

/// `Error::WeightsFileMissing` (AGL-043 F5).
const WEIGHTS_FILE_MISSING: Msg = Msg {
    en: "{key}: no file {path}",
    uk: "{key}: немає файлу {path}",
};
