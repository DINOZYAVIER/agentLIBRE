//! `agl server start|stop|status` and the autostart: the project's
//! llama-server as a transient systemd user unit (D146-D150, AGL-015),
//! or, on the agent command's path, the inference engine as a background
//! child of agl (AGL-024).

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::llama::Llama;
use crate::backend::Backend;
use crate::cli::args::ServerAction;
use crate::error::Error;
use crate::forge::Workspace;
use crate::i18n::{Lang, m, server as texts};
use crate::settings::{ServerFile, ServerKind};
use crate::tr;
use crate::outln;

/// The name every unit of a llama-server starts with (D8); the project's
/// key follows after a dash.
pub const UNIT_PREFIX: &str = "agentLIBRE-llama-server";

/// `context` when `[server]` does not set it (D148).
const DEFAULT_CONTEXT: u32 = 16384;

/// The record of what the running server was launched with, beside the
/// engine log in the project's store directory (AGL-035, D1).
const STATE_FILE: &str = "server.state";

/// How long `start` waits for `/health` (D149).
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(500);

/// Hosts `start` can run llama-server on (D148).
const LOCAL_HOSTS: [&str; 2] = ["127.0.0.1", "localhost"];

/// The engine's binary name when `[server] engine` does not set one
/// (AGL-024, D5).
const DEFAULT_ENGINE: &str = "llama-server";

/// How long `Engine::shutdown` waits after `SIGTERM` before `SIGKILL`
/// (AGL-024, D3): the `GRACE` of the bash tool.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// What `/health` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Ready,
    /// The server answers but is not ready, as while the model loads.
    Loading,
    Down,
}

/// What a server command works on: the server of the project (AGL-015).
#[derive(Debug)]
pub struct Server {
    /// The unit of the project (D8, D12).
    pub unit: String,
    /// The unit of the project's whisper server, which `agl server stop`
    /// stops too (AGL-014).
    pub whisper_unit: String,
    /// The project's `[server]` over the user's, both kinds (AGL-036, D1):
    /// the kind is the Function's when its file declares a `[server]`.
    pub settings: ServerFile,
    pub url: String,
    /// The user's and the project's settings files, for hints.
    pub user_path: PathBuf,
    pub project_path: PathBuf,
    /// The project's store directory: the record of the launched server
    /// is there (AGL-035, D1).
    pub dir: PathBuf,
    /// The `[server]` table of the Function the run loads, with the file
    /// it came from (AGL-033, D1): when the file declares one, the table
    /// is the model side of the settings, self-contained.
    pub function_server: Option<(ServerFile, PathBuf)>,
}

impl Server {
    /// The server of `project`, with `settings` the project's `[server]`
    /// over the user's and `function_server` the `[server]` table of the
    /// run's Function (AGL-033, D1). A project that is not registered has
    /// no unit: an error (D14).
    pub fn of(
        project: &Workspace,
        settings: &ServerFile,
        user_path: &Path,
        function_server: Option<(ServerFile, PathBuf)>,
    ) -> Result<Server, Error> {
        let key = project.unit_key().ok_or_else(|| Error::ProjectNotRegistered {
            project: project.project.clone(),
        })?;
        let project_path = project.dir.join(crate::forge::MARKER_FILE);
        let mut server = Server {
            unit: unit_name(&key),
            whisper_unit: format!("{}-{key}", super::whisper::UNIT_PREFIX),
            settings: settings.clone(),
            url: String::new(),
            user_path: user_path.into(),
            project_path,
            dir: project.dir.clone(),
            function_server,
        };
        // The url of the effective kind (AGL-036, D2, D6).
        server.url = server
            .effective_settings()?
            .url()
            .ok_or_else(|| Error::MissingServerUrl {
                user_path: user_path.into(),
                project_path: server.project_path.clone(),
            })?;
        Ok(server)
    }

    /// The settings that actually run (AGL-033, D1, D3; AGL-036, D1): the
    /// Function's `[server]` table when the file declares it
    /// (self-contained, with its kind), else the resolved settings of
    /// their kind (the fallback).
    pub fn effective_settings(&self) -> Result<ServerFile, Error> {
        match &self.function_server {
            Some((table, path)) => self.settings.effective(table, path),
            None => Ok(self.settings.selected()),
        }
    }

    /// What the server is, in a line (AGL-033, D2): the effective
    /// settings, by `what_of`.
    pub fn what(&self) -> Result<String, Error> {
        let effective = self.effective_settings()?;
        what_of(&effective)
    }
}

/// The unit of the project whose unit key is `key` (D8).
pub fn unit_name(key: &str) -> String {
    format!("{UNIT_PREFIX}-{key}")
}

/// The consent of the server commands (D9, D13; AGL-033, D2): what a
/// question gets — the start of a server that another unit holds, or the
/// restart of D2.
pub enum Others<'a> {
    /// Ask the human; `true` agrees.
    Ask(&'a dyn Fn(&str, Lang) -> bool),
    /// `--yes`: agree without asking.
    Yes,
    /// Nobody can answer and there is no `--yes`: refuse.
    Refuse,
    /// No question at all (AGL-033): agree without asking, as the port
    /// held by our own unit or by nothing.
    Skip,
}

/// Runs a server command. `models` is the models directory.
pub fn run(
    action: ServerAction,
    server: &Server,
    models: &Path,
    path_var: Option<OsString>,
    yes: bool,
    lang: Lang,
) -> Result<ExitCode, Error> {
    let llama = Llama::new(&server.url);
    match action {
        ServerAction::Start => {
            let others = if yes {
                Others::Yes
            } else if crate::repl::term::is_interactive() {
                Others::Ask(&ask_terminal)
            } else {
                Others::Refuse
            };
            start(
                server,
                models,
                path_var,
                &llama,
                &others,
                &mut |line| outln!("{line}"),
                lang,
            )
            .map(|()| ExitCode::SUCCESS)
        }
        ServerAction::Stop => {
            // The whisper server of the project goes with it (AGL-014);
            // nothing is said of it when it did not run.
            let whisper = unit_active(&server.whisper_unit)?;
            if whisper {
                systemctl(&["stop", &server.whisper_unit])?;
            }
            if unit_active(&server.unit)? {
                systemctl(&["stop", &server.unit])?;
                remove_record(&server.dir);
                println!("{}", tr!(lang, m::SERVER_STOPPED));
            } else if !whisper {
                outln!("{}", texts::not_running(&server.unit, lang));
            }
            if whisper {
                outln!("{}", tr!(lang, m::WHISPER_STOPPED));
            }
            Ok(ExitCode::SUCCESS)
        }
        ServerAction::Status => {
            let active = unit_active(&server.unit)?;
            let health = health(&llama);
            // The actually-running model (AGL-033, D5): both kinds of
            // server expose /v1/models; when the server does not answer
            // there is none.
            let running = (health == Health::Ready)
                .then(|| llama.models().unwrap_or(None))
                .flatten();
            // The model of the effective settings, as the server names
            // it (AGL-033, D5): the same identity the check of D2
            // compares — the full path of the llama kind, the config's
            // model_name of the strata kind.
            let model = expected_model(&server.effective_settings()?, models)?;
            outln!(
                "{}",
                texts::status(
                    &server.unit,
                    active,
                    &server.url,
                    model.as_deref(),
                    running.as_deref(),
                    health,
                    lang
                )
            );
            Ok(if health == Health::Ready {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

/// Starts the unit of the project and waits until the server is ready
/// (D146, D149); the same code as the autostart (AGL-015, D10). The
/// server of the strata kind is the Strata python server of the
/// `strata_config` (AGL-033, D2). When a server already answers at the
/// url, the check of D2 decides (AGL-033): it serves the effective
/// settings, or a mismatch restarts it with the consent as today.
pub fn start(
    server: &Server,
    models: &Path,
    path_var: Option<OsString>,
    llama: &Llama,
    others: &Others,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let url = &server.url;
    let effective = server.effective_settings()?;
    let (host, port) =
        local_address(url).ok_or_else(|| Error::NotLocalServer { url: url.into() })?;
    let what = server.what()?;
    // The server of the other kind that agl owns stops first (AGL-036,
    // D10): the two do not fit the GPU together.
    switch_kind(&server.dir, Some(&server.unit), None, (url, &what), others, say, lang)?;
    // A server answers at the url, this project's unit or a foreign one:
    // wait for it to be ready, then the check of D2 (AGL-033) — it
    // serves the effective settings, or a mismatch restarts it.
    if health(llama) != Health::Down || port_open(&host, port) {
        let ours = unit_active(&server.unit)?;
        wait_ready(llama, ours.then_some(&server.unit), &what, say, lang)?;
        if serves(llama, read_record(&server.dir).as_ref(), &effective, models)? {
            say(&texts::already_running(&what, url, lang));
            return Ok(());
        }
        // A mismatch (or a deviating key): the restart consent of D2
        // (AGL-033) — the existing ask / `--yes` / refuse — then the
        // server stops: the unit, or the units that run (a foreign
        // process agl cannot stop, D7).
        let running = active_units()?;
        restart_consent(&what, others, lang)?;
        if ours {
            systemctl(&["stop", &server.unit])?;
        } else if running.is_empty() {
            return Err(Error::PortBusy {
                url: url.into(),
                units: Vec::new(),
            });
        } else {
            for unit in &running {
                systemctl(&["stop", unit])?;
            }
        }
        remove_record(&server.dir);
        let started = Instant::now();
        while port_open(&host, port) {
            if started.elapsed() >= Duration::from_secs(10) {
                return Err(Error::PortBusy {
                    url: url.into(),
                    units: running,
                });
            }
            thread::sleep(POLL);
        }
    }
    // Nothing at the url: the other units that run are named, there may
    // be none (D9, D13) — with none, no question is asked; then the
    // server of the kind starts with the effective settings (AGL-033,
    // D1, D2).
    let running = active_units()?;
    if running.is_empty() {
        launch_unit(server, &effective, models, path_var, llama, say, lang)
    } else {
        consent(&texts::others_running(&running, lang), others, lang)?;
        launch_unit(server, &effective, models, path_var, llama, say, lang)
    }
}

/// The server of the kind starts as the unit of the project with the
/// effective settings (AGL-033, D2): the llama engine or the Strata
/// python server, then the wait for ready (D149).
fn launch_unit(
    server: &Server,
    effective: &ServerFile,
    models: &Path,
    path_var: Option<OsString>,
    llama: &dyn Backend,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let url = &server.url;
    let what = what_of(effective)?;
    let (host, port) =
        local_address(url).ok_or_else(|| Error::NotLocalServer { url: url.into() })?;
    let args = match effective.kind() {
        ServerKind::Strata => {
            let launch = strata_launch(
                effective,
                url,
                &server.user_path,
                &server.project_path,
            )?;
            strata_systemd_run_args(&server.unit, &launch)
        }
        ServerKind::Llama => {
            let model = effective
                .model
                .as_deref()
                .ok_or_else(|| Error::MissingModel {
                    user_path: server.user_path.clone(),
                    project_path: server.project_path.clone(),
                    models: models.into(),
                })?;
            let model = models.join(model);
            if !model.is_file() {
                return Err(Error::ModelNotFound { path: model });
            }
            let program = find_program(DEFAULT_ENGINE, path_var).ok_or_else(|| {
                Error::EngineNotFound {
                    engine: DEFAULT_ENGINE.to_owned(),
                }
            })?;
            let context = effective.context.unwrap_or(DEFAULT_CONTEXT);
            let extra = effective.args.clone().unwrap_or_default();
            let projector = projector_args(effective, models)?;
            let spec = spec(effective, models)?;
            systemd_run_args(
                &server.unit,
                &program,
                &model,
                (context, &host, port),
                &projector,
                &spec,
                &extra,
            )
        }
    };
    let output = Command::new("systemd-run")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Systemd {
            detail: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::Systemd {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    say(&texts::starting(&what, url, &server.unit, lang));
    wait_ready(llama, Some(&server.unit), &what, say, lang)?;
    // What the unit was launched with (AGL-035, D1).
    if let Some(pid) = unit_main_pid(&server.unit) {
        write_record(&server.dir, pid, effective)?;
    }
    Ok(())
}

/// Waits until `/health` answers ready (D149). `unit` is the unit that
/// loads the model, when it is ours: it stopping is a failure. `what` is
/// the server, in a line (AGL-033, D2).
pub(super) fn wait_ready(
    llama: &dyn Backend,
    unit: Option<&str>,
    what: &str,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let started = Instant::now();
    loop {
        if health(llama) == Health::Ready {
            say(&tr!(lang, m::SERVER_READY, what = what, url = llama.url()));
            return Ok(());
        }
        if let Some(unit) = unit
            && !unit_active(unit)?
        {
            return Err(Error::ServerFailed { unit: unit.into() });
        }
        if started.elapsed() >= READY_TIMEOUT {
            return Err(Error::ServerTimeout {
                seconds: READY_TIMEOUT.as_secs(),
                unit: unit.unwrap_or(UNIT_PREFIX).into(),
            });
        }
        thread::sleep(POLL);
    }
}

/// Brings the project's server up when a request finds none (AGL-015,
/// D6, D10); the same code as `agl server start`: the check of D2
/// (AGL-033) beside the health check, the launch of both kinds, the
/// consent of the restart.
pub struct Autostart<'a> {
    /// The backend the turn runs on: the `Llama` or the `Strata` backend,
    /// by the settings' `[server] kind` (AGL-030, D1).
    pub llama: &'a dyn Backend,
    pub project: &'a Workspace,
    /// The settings that actually run (AGL-033, D1): the function's
    /// `[server]` table when the file declares it, else `settings`.
    pub effective: &'a ServerFile,
    /// The project's `[whisper]` over the user's (AGL-014, D22).
    pub whisper: &'a crate::settings::WhisperFile,
    pub user_path: PathBuf,
    pub models: PathBuf,
    pub path_var: Option<OsString>,
    pub lang: Lang,
    /// The consent of D2 (AGL-033): the restart asks, as the server
    /// commands do.
    pub others: Others<'a>,
    /// The engine child agl launched itself (AGL-024, D2, D3): `None`
    /// while agl uses a server it does not own.
    pub engine: Arc<Mutex<Option<Engine>>>,
}

impl Autostart<'_> {
    /// The server, in a line (AGL-033, D2): the effective settings, by
    /// `what_of`.
    fn what(&self) -> Result<String, Error> {
        what_of(self.effective)
    }

    /// Returns when the server answers and serves the effective settings
    /// (AGL-033, D2): it did, it was loading and is ready now and serves,
    /// or agl launched the server of the kind itself as a background
    /// child (AGL-024, D2) — restarting the one that does not serve,
    /// with the consent as today. `say` takes the progress lines.
    pub fn ensure(&self, say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        let llama = self.llama;
        let what = self.what()?;
        // The server of the other kind that agl owns stops first
        // (AGL-036, D10): the two do not fit the GPU together.
        self.switch(say)?;
        match health(llama) {
            // The server is ready: the check of D2 (AGL-033) decides —
            // it serves the effective settings, or a mismatch restarts.
            Health::Ready => {
                if serves(llama, read_record(&self.project.dir).as_ref(), self.effective, &self.models)? {
                    Ok(())
                } else {
                    self.restart(&what, say)
                }
            }
            // Someone loads it now: wait for it, as `start` does, then
            // the check of D2 (AGL-033).
            Health::Loading => {
                wait_ready(llama, None, &what, say, self.lang)?;
                if serves(llama, read_record(&self.project.dir).as_ref(), self.effective, &self.models)? {
                    Ok(())
                } else {
                    self.restart(&what, say)
                }
            }
            // Connection refused: launch the server of the kind as a
            // background child (AGL-024, D2; AGL-033, D2 — both kinds);
            // a foreign process on the port is an error, not a target.
            Health::Down => self.launch(&what, say),
        }
    }

    /// The switch of D10 (AGL-036): the server of the other kind that agl
    /// owns stops, with the consent. `ensure` runs it; the interactive
    /// session runs it before it takes the terminal, where a question can
    /// still be answered.
    pub fn switch(&self, say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        let unit = self.project.unit_key().map(|key| unit_name(&key));
        switch_kind(
            &self.project.dir,
            unit.as_deref(),
            Some(&self.engine),
            (self.llama.url(), &self.what()?),
            &self.others,
            say,
            self.lang,
        )
    }

    /// The mismatch of D2 (AGL-033): the consent as today (ask / `--yes`
    /// / refuse), then the server agl owns stops and the server of the
    /// kind starts with the effective settings, with the owner of the one
    /// it replaced (AGL-035, D2): the project's unit stays the unit, the
    /// run's own child stays the child.
    fn restart(&self, what: &str, say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        restart_consent(what, &self.others, self.lang)?;
        if self.stop_ours()? {
            self.launch_as_unit(say)
        } else {
            self.launch(what, say)
        }
    }

    /// The server of the kind starts as the unit of the project, the code
    /// of `agl server start` (AGL-035, D2).
    fn launch_as_unit(&self, say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        let server = Server::of(self.project, self.effective, &self.user_path, None)?;
        launch_unit(
            &server,
            self.effective,
            &self.models,
            self.path_var.clone(),
            self.llama,
            say,
            self.lang,
        )
    }

    /// The server agl owns stops (AGL-033, D2): the child in the field
    /// (AGL-024, D3), the unit of the project when it is active; then
    /// the port must be free, or it is busy (D7). Whether it was the unit
    /// (AGL-035, D2).
    fn stop_ours(&self) -> Result<bool, Error> {
        if let Some(engine) = self.engine.lock().unwrap().take() {
            engine.shutdown();
        }
        remove_record(&self.project.dir);
        let unit = self.project.unit_key().map(|key| unit_name(&key));
        let mut was_unit = false;
        if let Some(unit) = &unit
            && unit_active(unit)?
        {
            systemctl(&["stop", unit])?;
            was_unit = true;
        }
        let url = self.llama.url();
        let Some((host, port)) = local_address(url) else {
            return Ok(was_unit);
        };
        let started = Instant::now();
        while port_open(&host, port) {
            if started.elapsed() >= Duration::from_secs(10) {
                return Err(Error::PortBusy {
                    url: url.into(),
                    units: Vec::new(),
                });
            }
            thread::sleep(POLL);
        }
        Ok(was_unit)
    }

    /// The server of the kind starts as a background child with the
    /// effective settings (AGL-033, D2): the llama engine, or the Strata
    /// python server from the config JSON; the child is kept in the field
    /// (AGL-024, D2, D3) and the wait for ready (D149).
    fn launch(&self, what: &str, say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        let project_path = self.project.dir.join(crate::forge::MARKER_FILE);
        let (engine, log) = match self.effective.kind() {
            ServerKind::Strata => {
                let launch =
                    strata_launch(self.effective, self.llama.url(), &self.user_path, &project_path)?;
                // The log beside the config when it names one, else beside
                // the project.
                let log = launch
                    .config
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|dir| dir.join("strata.log"))
                    .unwrap_or_else(|| self.project.dir.join("server.log"));
                (Engine::launch_strata(&launch, &log)?, log)
            }
            ServerKind::Llama => {
                let log = self.project.dir.join("server.log");
                let engine = Engine::launch(
                    self.effective,
                    self.llama.url(),
                    &self.user_path,
                    &project_path,
                    &self.models,
                    self.path_var.clone(),
                    &log,
                )?;
                (engine, log)
            }
        };
        let mut guard = self.engine.lock().unwrap();
        *guard = Some(engine);
        let engine = guard.as_mut().expect("the engine just launched");
        wait_child_ready(self.llama, engine, &log, what, say, self.lang)?;
        // What the child was launched with (AGL-035, D1).
        write_record(&self.project.dir, engine.pid(), self.effective)
    }
}

/// The inference engine agl launched itself as a background child
/// (AGL-024, D2, D6): the child and how to end it (D3).
pub struct Engine {
    child: Child,
    /// Whether the child is already reaped (by `is_alive` or `shutdown`).
    dead: bool,
}

impl Engine {
    /// Launches the engine of `settings` on `url` (AGL-024, D2, D5):
    /// the binary the `engine` key names (default `llama-server`), found
    /// on `path_var`; the model in `models`; the arguments of the
    /// `[server]` settings; the child's output to the log file `log`,
    /// appended on each launch. A foreign process on the port is an
    /// error, not a target (D2).
    pub fn launch(
        settings: &ServerFile,
        url: &str,
        user_path: &Path,
        project_path: &Path,
        models: &Path,
        path_var: Option<OsString>,
        log: &Path,
    ) -> Result<Engine, Error> {
        let (host, port) =
            local_address(url).ok_or_else(|| Error::NotLocalServer { url: url.into() })?;
        if port_open(&host, port) {
            return Err(Error::PortBusy {
                url: url.into(),
                units: Vec::new(),
            });
        }
        let model = settings
            .model
            .as_deref()
            .ok_or_else(|| Error::MissingModel {
                user_path: user_path.into(),
                project_path: project_path.into(),
                models: models.into(),
            })?;
        let model = models.join(model);
        if !model.is_file() {
            return Err(Error::ModelNotFound { path: model });
        }
        let engine = settings
            .engine
            .as_deref()
            .unwrap_or(DEFAULT_ENGINE)
            .to_owned();
        let program = find_program(&engine, path_var)
            .ok_or_else(|| Error::EngineNotFound { engine: engine.clone() })?;
        let context = settings.context.unwrap_or(DEFAULT_CONTEXT);
        let extra = settings.args.clone().unwrap_or_default();
        let projector = projector_args(settings, models)?;
        let spec = spec(settings, models)?;
        let log_out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|source| Error::Write {
                path: log.into(),
                source,
            })?;
        let log_err = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|source| Error::Write {
                path: log.into(),
                source,
            })?;
        // `engine_args` is the single source of the command (D4): the
        // program first, then the arguments. `Command::new` takes the
        // program, the rest are the arguments.
        let command =
            engine_args(&program, &model, (context, &host, port), &projector, &spec, &extra);
        let child = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err))
            .spawn()
            .map_err(|source| Error::EngineSpawn {
                engine,
                source,
            })?;
        Ok(Engine {
            child,
            dead: false,
        })
    }

    /// The child's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the child runs; a reaped child is not (the exit status is
    /// taken here, so `shutdown` does not wait on it again).
    pub fn is_alive(&mut self) -> bool {
        if self.dead {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.dead = true;
                false
            }
            Ok(None) => true,
            // The child is gone (as `ECHILD` says): not alive.
            Err(_) => {
                self.dead = true;
                false
            }
        }
    }

    /// Ends the child (AGL-024, D3): `SIGTERM`, the grace of the bash
    /// tool, then `SIGKILL`.
    pub fn shutdown(mut self) {
        if self.dead {
            return;
        }
        let pid = self.child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while self.is_alive() {
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                let _ = self.child.wait();
                self.dead = true;
                return;
            }
            thread::sleep(SHUTDOWN_POLL);
        }
    }
}

/// What the server is, in a line (AGL-033, D2): `llama-server` with
/// the model, the Strata server with the config's `model_name`.
fn what_of(effective: &ServerFile) -> Result<String, Error> {
    let model = model_of(effective)?;
    Ok(match (effective.kind(), model) {
        (ServerKind::Strata, Some(name)) => format!("the Strata server ({name})"),
        (ServerKind::Strata, None) => "the Strata server".to_owned(),
        (ServerKind::Llama, Some(name)) => format!("llama-server ({name})"),
        (ServerKind::Llama, None) => "llama-server".to_owned(),
    })
}

/// The model of the settings, by its name (AGL-033, D2; AGL-036, D3): the
/// model file of the llama kind, the config's `model_name` of the strata
/// kind; `None` when the settings name none.
pub(crate) fn model_of(settings: &ServerFile) -> Result<Option<String>, Error> {
    match settings.kind() {
        ServerKind::Strata => Ok(settings
            .strata_config
            .as_deref()
            .map(|path| strata_config(Path::new(path)))
            .transpose()?
            .and_then(|cfg| cfg.model_name)),
        ServerKind::Llama => Ok(settings.model.clone()),
    }
}

/// The Strata config JSON the `strata_config` names (AGL-033, D2): the
/// parts of the launch agl needs; the engine's binary, its arguments,
/// the HIP libraries and the environment are applied by the server
/// itself.
struct StrataConfig {
    /// The `model_name` the server reports at `/v1/models`.
    model_name: Option<String>,
    /// The working directory (`cwd`), inherited from agl when unset.
    cwd: Option<PathBuf>,
}

/// Reads the Strata config JSON at `path` (AGL-033, D2).
fn strata_config(path: &Path) -> Result<StrataConfig, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::StrataConfig {
        path: path.into(),
        detail: source.to_string(),
    })?;
    let cfg: serde_json::Value = serde_json::from_str(&text)
        .map_err(|source| Error::StrataConfig {
            path: path.into(),
            detail: source.to_string(),
        })?;
    Ok(StrataConfig {
        model_name: cfg["model_name"].as_str().map(str::to_owned),
        cwd: cfg["cwd"].as_str().map(PathBuf::from),
    })
}

/// The launch of the Strata server (AGL-033, D2): the venv python of the
/// repo the config is in, the `serve/server.py` beside it, the port of
/// the url, and the config's cwd.
pub struct StrataLaunch {
    python: PathBuf,
    server_py: PathBuf,
    config: PathBuf,
    port: u16,
    cwd: Option<PathBuf>,
}

/// Resolves the launch of the Strata server of `settings` at `url`
/// (AGL-033, D2): the `strata_config` names the config JSON, the python
/// is the `.venv` beside it, the server the `serve/server.py` beside it,
/// and the port is the url's. A foreign process on the port is an error,
/// not a target.
fn strata_launch(
    settings: &ServerFile,
    url: &str,
    user_path: &Path,
    project_path: &Path,
) -> Result<StrataLaunch, Error> {
    let (host, port) =
        local_address(url).ok_or_else(|| Error::NotLocalServer { url: url.into() })?;
    if port_open(&host, port) {
        return Err(Error::PortBusy {
            url: url.into(),
            units: Vec::new(),
        });
    }
    let Some(config) = &settings.strata_config else {
        return Err(Error::MissingStrataConfig {
            user_path: user_path.into(),
            project_path: project_path.into(),
        });
    };
    let config = PathBuf::from(config);
    let cfg = strata_config(&config)?;
    let config_dir = config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_default();
    let python = config_dir.join(".venv/bin/python");
    if !python.is_file() {
        return Err(Error::StrataPython { path: python });
    }
    let server_py = config_dir.join("serve/server.py");
    if !server_py.is_file() {
        return Err(Error::StrataServerPy { path: server_py });
    }
    Ok(StrataLaunch {
        python,
        server_py,
        config,
        port,
        cwd: cfg.cwd,
    })
}

/// The arguments of the Strata server, the program first (AGL-033, D2):
/// the venv python, the `serve/server.py`, `--engine strata`, the config
/// and the port.
fn strata_args(launch: &StrataLaunch) -> Vec<OsString> {
    let mut args = vec![
        launch.python.clone().into_os_string(),
        launch.server_py.clone().into_os_string(),
    ];
    args.extend(
        [
            "--engine",
            "strata",
            "--config",
            launch.config.to_str().unwrap_or_default(),
            "--port",
            &launch.port.to_string(),
        ]
        .iter()
        .map(OsString::from),
    );
    args
}

/// The arguments of `systemd-run` for the Strata server (AGL-033, D2):
/// the unit, the config's cwd as a property, then the server.
fn strata_systemd_run_args(unit: &str, launch: &StrataLaunch) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--user",
        &format!("--unit={unit}"),
        "--collect",
        "--quiet",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    if let Some(cwd) = &launch.cwd {
        args.push(OsString::from(format!(
            "--property=WorkingDirectory={}",
            cwd.display()
        )));
    }
    args.push("--".into());
    args.extend(strata_args(launch));
    args
}

impl Engine {
    /// Launches the Strata server of the launch as a background child
    /// (AGL-033, D2): the venv python running `serve/server.py` with
    /// `--engine strata --config` the config and `--port` the port, from
    /// the config's cwd; the child's output to the log file `log`,
    /// appended on each launch.
    pub fn launch_strata(launch: &StrataLaunch, log: &Path) -> Result<Engine, Error> {
        let log_out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|source| Error::Write {
                path: log.into(),
                source,
            })?;
        let log_err = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|source| Error::Write {
                path: log.into(),
                source,
            })?;
        let args = strata_args(launch);
        // The chain after `Command::new` returns `&mut Command`, so the
        // `Command` itself must outlive the statement: bind it first.
        let mut command = Command::new(&args[0]);
        command
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_out))
            .stderr(Stdio::from(log_err));
        if let Some(cwd) = &launch.cwd {
            command.current_dir(cwd);
        }
        let child = command.spawn().map_err(|source| {
            Error::EngineSpawn {
                engine: "the Strata server".to_owned(),
                source,
            }
        })?;
        Ok(Engine {
            child,
            dead: false,
        })
    }
}

/// The identity of the model the server at the url must serve (AGL-033,
/// D2): the full path of the model (the llama kind), the `model_name` of
/// the Strata config (the strata kind); `None` when the settings name no
/// model.
fn expected_model(settings: &ServerFile, models: &Path) -> Result<Option<String>, Error> {
    match settings.kind() {
        ServerKind::Llama => Ok(settings
            .model
            .as_deref()
            .map(|name| models.join(name).to_string_lossy().into_owned())),
        ServerKind::Strata => Ok(settings
            .strata_config
            .as_deref()
            .map(|path| strata_config(Path::new(path)))
            .transpose()?
            .and_then(|cfg| cfg.model_name)),
    }
}

/// The check of D2 (AGL-033): whether the server running at the url
/// serves the effective settings. The model is observed via `/v1/models`
/// (both servers expose it); a mismatch of the observed model, or a
/// model-side key of the effective settings that deviates from `record`
/// — what the running server was launched with, which is the only way to
/// know the context (AGL-035, D1) — is a restart. Without a record the
/// observed model decides alone.
fn serves(
    llama: &dyn Backend,
    record: Option<&ServerFile>,
    effective: &ServerFile,
    models: &Path,
) -> Result<bool, Error> {
    if let Some(record) = record
        && effective.model_side_differs(record)
    {
        return Ok(false);
    }
    let expected = expected_model(effective, models)?;
    let Some(expected) = expected else {
        return Ok(true);
    };
    // The server answers but does not name its model: nothing to
    // compare, no restart.
    let observed = llama.models().unwrap_or(None);
    match observed {
        Some(observed) => Ok(observed == expected),
        None => Ok(true),
    }
}

/// The record of what the running server was launched with (AGL-035,
/// D1): `pid` and the effective `[server]`, in the format of a settings
/// file.
fn write_record(dir: &Path, pid: u32, effective: &ServerFile) -> Result<(), Error> {
    let path = dir.join(STATE_FILE);
    fs::write(&path, format!("pid = {pid}\n{}", effective.render()))
        .map_err(|source| Error::Write { path, source })
}

/// What the running server was launched with (AGL-035, D1): `None` when
/// there is no record, it cannot be read, or its process is gone.
fn read_record(dir: &Path) -> Option<ServerFile> {
    let path = dir.join(STATE_FILE);
    let table = crate::tomlfile::read(&path).ok()??;
    let mut file = crate::tomlfile::Section::new(&path, table);
    file.check_keys(&["pid", "server"]).ok()?;
    let pid = file.integer("pid").ok()??;
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return None;
    }
    ServerFile::parse(file.section("server").ok()?, &path).ok()
}

/// The record goes away with the server it describes (AGL-035, D1).
pub(crate) fn remove_record(dir: &Path) {
    let _ = fs::remove_file(dir.join(STATE_FILE));
}

/// The main process of the unit, while it runs.
fn unit_main_pid(unit: &str) -> Option<u32> {
    let output = Command::new("systemctl")
        .args(["--user", "show", "--property=MainPID", "--value", unit])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()
        .filter(|pid| *pid != 0)
}

/// Waits until the engine agl launched answers ready (AGL-024, D2): the
/// same timeout and polling as `wait_ready`; the engine's own output is
/// in the log file the errors point at. `what` is the server, in a line
/// (AGL-033, D2).
fn wait_child_ready(
    llama: &dyn Backend,
    engine: &mut Engine,
    log: &Path,
    what: &str,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let started = Instant::now();
    loop {
        if health(llama) == Health::Ready {
            say(&tr!(lang, m::SERVER_READY, what = what, url = llama.url()));
            return Ok(());
        }
        if !engine.is_alive() {
            return Err(Error::EngineDied { log: log.into() });
        }
        if started.elapsed() >= READY_TIMEOUT {
            return Err(Error::EngineTimeout {
                seconds: READY_TIMEOUT.as_secs(),
                log: log.into(),
            });
        }
        thread::sleep(POLL);
    }
}

/// The consent (D9, D13; AGL-033, D2): `question` once, the answer as
/// today — `Skip` and `--yes` agree, the terminal asks, and a
/// non-interactive terminal refuses.
fn consent(question: &str, others: &Others, lang: Lang) -> Result<(), Error> {
    match others {
        Others::Skip | Others::Yes => Ok(()),
        Others::Ask(ask) => {
            if ask(question, lang) {
                Ok(())
            } else {
                Err(Error::StartDeclined)
            }
        }
        Others::Refuse => Err(Error::ServerRunning { units: Vec::new() }),
    }
}

/// The restart consent of D2 (AGL-033): the same consent, with the
/// question of the mismatch; a refusal is the `RestartDeclined`.
fn restart_consent(what: &str, others: &Others, lang: Lang) -> Result<(), Error> {
    match consent(&texts::restart_question(what, lang), others, lang) {
        Ok(()) => Ok(()),
        Err(Error::ServerRunning { .. }) => Err(Error::RestartDeclined),
        Err(error) => Err(error),
    }
}

/// The server agl owns runs elsewhere than `url`, the url of the effective
/// kind (AGL-036, D10): the server of the other kind, which holds the GPU.
/// It is told by the record (AGL-035, D1) — its url is not `url` — or,
/// without a record, by the project's unit, active while nothing answers
/// at `url`. `None` when agl owns no server elsewhere.
fn owned_elsewhere(dir: &Path, unit: Option<&str>, url: &str) -> Result<Option<Elsewhere>, Error> {
    if let Some(record) = read_record(dir) {
        let Some(at) = record.url().filter(|at| at != url) else {
            return Ok(None);
        };
        return Ok(Some(Elsewhere {
            what: Some(what_of(&record)?),
            url: Some(at),
        }));
    }
    let Some(unit) = unit else {
        return Ok(None);
    };
    let free = local_address(url).is_none_or(|(host, port)| !port_open(&host, port));
    Ok((free && unit_active(unit)?).then_some(Elsewhere {
        what: None,
        url: None,
    }))
}

/// The server agl owns elsewhere (AGL-036, D10): what it is and where,
/// when the record says.
struct Elsewhere {
    what: Option<String>,
    url: Option<String>,
}

/// The switch of D10 (AGL-036): when the server agl owns runs elsewhere
/// than `url` — the other kind's — it stops with the consent (ask /
/// `--yes` / refuse) before the server of the effective kind, `what`,
/// starts: the child in `engine` and the project's `unit`; then its port
/// must be free. A foreign server is not touched.
fn switch_kind(
    dir: &Path,
    unit: Option<&str>,
    engine: Option<&Mutex<Option<Engine>>>,
    (url, what): (&str, &str),
    others: &Others,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let Some(elsewhere) = owned_elsewhere(dir, unit, url)? else {
        return Ok(());
    };
    let old = elsewhere
        .what
        .unwrap_or_else(|| tr!(lang, m::SERVER_OF_THE_PROJECT).to_owned());
    let at = elsewhere.url.as_deref().unwrap_or("?");
    match consent(&texts::switch_question(&old, at, what, lang), others, lang) {
        Ok(()) => {}
        Err(Error::ServerRunning { .. }) => return Err(Error::SwitchDeclined),
        Err(error) => return Err(error),
    }
    if let Some(engine) = engine.and_then(|engine| engine.lock().unwrap().take()) {
        engine.shutdown();
    }
    if let Some(unit) = unit
        && unit_active(unit)?
    {
        systemctl(&["stop", unit])?;
    }
    remove_record(dir);
    if let Some((host, port)) = elsewhere.url.as_deref().and_then(local_address) {
        let started = Instant::now();
        while port_open(&host, port) {
            if started.elapsed() >= Duration::from_secs(10) {
                return Err(Error::PortBusy {
                    url: at.into(),
                    units: Vec::new(),
                });
            }
            thread::sleep(POLL);
        }
    }
    say(&tr!(lang, m::SERVER_SWITCH_STOPPED, old = old));
    Ok(())
}

/// The answer to a question on a terminal (D9, D13; AGL-033): the
/// question is a line, the answer a line of stdin, `y` or `yes` agrees.
pub(crate) fn ask_terminal(question: &str, _lang: Lang) -> bool {
    print!("{question} ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    is_yes(&line)
}

/// Whether the answer to a yes/no question is yes.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

pub(super) fn health(llama: &dyn Backend) -> Health {
    match llama.health() {
        Some(200) => Health::Ready,
        Some(_) => Health::Loading,
        None => Health::Down,
    }
}

/// Whether something accepts connections at `host:port`.
pub(super) fn port_open(host: &str, port: u16) -> bool {
    (host, port)
        .to_socket_addrs()
        .into_iter()
        .flatten()
        .any(|address: SocketAddr| {
            TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
        })
}

/// Whether the unit runs.
pub(super) fn unit_active(unit: &str) -> Result<bool, Error> {
    let status = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .stdin(Stdio::null())
        .status()
        .map_err(|source| Error::Systemd {
            detail: source.to_string(),
        })?;
    Ok(status.success())
}

/// The active llama-server units of every project, by name (D9).
pub fn active_units() -> Result<Vec<String>, Error> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--plain",
            "--no-legend",
            "--no-pager",
            "--state=active",
            "--type=service",
            &format!("{UNIT_PREFIX}*"),
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Systemd {
            detail: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::Systemd {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    Ok(parse_units(&String::from_utf8_lossy(&output.stdout)))
}

/// The unit names of `systemctl list-units --plain --no-legend`: the
/// first column of every line, without `.service`.
fn parse_units(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| name.strip_suffix(".service").unwrap_or(name).to_owned())
        .collect()
}

fn systemctl(args: &[&str]) -> Result<(), Error> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Systemd {
            detail: source.to_string(),
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Systemd {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        })
    }
}

/// The host and port of an `http://` URL whose host is local (D148).
pub(super) fn local_address(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80),
    };
    LOCAL_HOSTS.contains(&host).then(|| (host.to_owned(), port))
}

/// The projector arguments of `[server]`: its file, where it runs and
/// the most tokens an image takes (AGL-014, D9, D23).
/// The speculative part of the launch (AGL-025, D2): the type and the
/// resolved draft model; both `None` when the settings set none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Spec {
    spec_type: Option<String>,
    draft: Option<PathBuf>,
}

/// The speculative decoding of the `[server]` settings (AGL-025, D2, D4):
/// the `draft` must be a file in the models directory, and it needs a
/// `spec_type` — the build's `--spec-type` default is `none`, so a draft
/// model alone would not run.
pub(super) fn spec(settings: &ServerFile, models: &Path) -> Result<Spec, Error> {
    let draft = match &settings.draft {
        Some(name) => {
            let path = models.join(name);
            if !path.is_file() {
                return Err(Error::DraftNotFound { path });
            }
            Some(path)
        }
        None => None,
    };
    if draft.is_some() && settings.spec_type.is_none() {
        return Err(Error::MissingSpecType {
            draft: settings.draft.clone().unwrap_or_default(),
        });
    }
    Ok(Spec {
        spec_type: settings.spec_type.clone(),
        draft,
    })
}

pub(super) fn projector_args(settings: &ServerFile, models: &Path) -> Result<Vec<OsString>, Error> {
    let mut projector: Vec<OsString> = Vec::new();
    if let Some(name) = &settings.mmproj {
        let file = models.join(name);
        if !file.is_file() {
            return Err(Error::ModelNotFound { path: file });
        }
        projector.extend(["--mmproj".into(), file.into_os_string()]);
        if settings.mmproj_offload == Some(false) {
            projector.push("--no-mmproj-offload".into());
        }
        let tokens = settings
            .image_max_tokens
            .unwrap_or(crate::settings::DEFAULT_IMAGE_TOKENS);
        projector.extend(["--image-max-tokens".into(), tokens.to_string().into()]);
    }
    Ok(projector)
}

/// The first executable `name` in `path_var` (D147).
pub(super) fn find_program(name: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The arguments of the inference engine: the program, the model, the
/// context, the local address, `--jinja`, one process, the projector,
/// the speculative decoding, the extra of `[server] args` (AGL-024, D4;
/// AGL-025, D2): the one source of the launch arguments, the systemd
/// and the child paths alike.
pub(super) fn engine_args(
    engine: &Path,
    model: &Path,
    (context, host, port): (u32, &str, u16),
    projector: &[OsString],
    spec: &Spec,
    extra: &[String],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    args.push(engine.into());
    args.push("-m".into());
    args.push(model.into());
    for arg in [
        "-c",
        &context.to_string(),
        "--host",
        host,
        "--port",
        &port.to_string(),
        "--jinja",
        "-np",
        "1",
    ] {
        args.push(arg.into());
    }
    args.extend(projector.iter().cloned());
    if let Some(spec_type) = &spec.spec_type {
        args.extend(["--spec-type".into(), spec_type.as_str().into()]);
    }
    if let Some(draft) = &spec.draft {
        args.extend(["--spec-draft-model".into(), draft.as_os_str().to_os_string()]);
    }
    args.extend(extra.iter().map(OsString::from));
    args
}

/// The arguments of `systemd-run`: `unit`, then the engine with the
/// arguments of `engine_args` (D147, D148, AGL-024).
fn systemd_run_args(
    unit: &str,
    engine: &Path,
    model: &Path,
    (context, host, port): (u32, &str, u16),
    projector: &[OsString],
    spec: &Spec,
    extra: &[String],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--user",
        &format!("--unit={unit}"),
        "--collect",
        "--quiet",
        "--",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.extend(engine_args(
        engine, model, (context, host, port), projector, spec, extra,
    ));
    args
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn status_lines() {
        assert_eq!(
            texts::status(
                "agentLIBRE-llama-server-60af300abea6",
                true,
                "http://127.0.0.1:18080",
                None,
                None,
                Health::Loading,
                Lang::En
            ),
            "unit: agentLIBRE-llama-server-60af300abea6 (active)\nurl: http://127.0.0.1:18080\n\
             model: not set\nrunning: not running\nhealth: loading"
        );
        assert_eq!(
            texts::status(
                "agentLIBRE-llama-server-60af300abea6",
                false,
                "http://x",
                Some("q.gguf"),
                None,
                Health::Down,
                Lang::Uk
            ),
            "unit: agentLIBRE-llama-server-60af300abea6 (неактивний)\nадреса: http://x\n\
             модель: q.gguf\nпрацює: не працює\nстан: не відповідає"
        );
    }

    #[test]
    fn only_local_addresses() {
        assert_eq!(
            local_address("http://127.0.0.1:18080"),
            Some(("127.0.0.1".into(), 18080))
        );
        assert_eq!(
            local_address("http://localhost:8080/"),
            Some(("localhost".into(), 8080))
        );
        assert_eq!(
            local_address("http://localhost"),
            Some(("localhost".into(), 80))
        );
        assert_eq!(local_address("http://192.168.1.5:18080"), None);
        assert_eq!(local_address("http://127.0.0.1:port"), None);
    }

    #[test]
    fn llama_server_is_found_in_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agentLIBRE-server-{}", std::process::id()));
        let (plain, bin) = (dir.join("plain"), dir.join("bin"));
        fs::create_dir_all(&plain).unwrap();
        fs::create_dir_all(&bin).unwrap();
        // Not executable, so skipped.
        fs::write(plain.join("llama-server"), "").unwrap();
        let program = bin.join("llama-server");
        fs::write(&program, "").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([Path::new("relative"), &plain, &bin]).unwrap();
        let found = find_program("llama-server", Some(path));
        let missing = find_program("llama-server", Some(plain.clone().into_os_string()));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, Some(program));
        assert_eq!(missing, None);
        assert_eq!(find_program("llama-server", None), None);
    }

    #[test]
    fn the_unit_runs_llama_server_with_the_needed_arguments() {
        let args = systemd_run_args(
            "agentLIBRE-llama-server-60af300abea6",
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            (8192, "127.0.0.1", 18080),
            &[],
            &Spec::default(),
            &["-ngl".into(), "999".into()],
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user",
                "--unit=agentLIBRE-llama-server-60af300abea6",
                "--collect",
                "--quiet",
                "--",
                "/bin/llama-server",
                "-m",
                "/m/q.gguf",
                "-c",
                "8192",
                "--host",
                "127.0.0.1",
                "--port",
                "18080",
                "--jinja",
                "-np",
                "1",
                "-ngl",
                "999"
            ]
        );
    }

    #[test]
    fn the_projector_flags_come_before_the_extra_arguments() {
        let projector: Vec<OsString> = [
            "--mmproj",
            "/m/p.gguf",
            "--no-mmproj-offload",
            "--image-max-tokens",
            "2048",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let args = systemd_run_args(
            "agentLIBRE-llama-server-60af300abea6",
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            (65536, "127.0.0.1", 18080),
            &projector,
            &Spec::default(),
            &["-ngl".into(), "999".into()],
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        let tail = &args[args.len() - 7..];
        assert_eq!(
            tail,
            ["--mmproj", "/m/p.gguf", "--no-mmproj-offload", "--image-max-tokens", "2048", "-ngl", "999"]
        );
        assert!(args.contains(&"65536"));
    }

    #[test]
    fn engine_args_are_the_systemd_tail_after_the_prefix() {
        let systemd = systemd_run_args(
            "agentLIBRE-llama-server-60af300abea6",
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            (8192, "127.0.0.1", 18080),
            &[],
            &Spec::default(),
            &["-ngl".into(), "999".into()],
        );
        let engine = engine_args(
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            (8192, "127.0.0.1", 18080),
            &[],
            &Spec::default(),
            &["-ngl".into(), "999".into()],
        );
        // The systemd prefix is `--user --unit=… --collect --quiet --`;
        // the engine arguments are the rest (AGL-024, D4).
        assert_eq!(systemd.len(), 5 + engine.len());
        let tail: Vec<&OsString> = systemd.iter().skip(5).collect();
        let engine: Vec<&OsString> = engine.iter().collect();
        assert_eq!(tail, engine);
    }

    #[test]
    fn the_speculative_flags_come_after_the_projector_before_the_extra() {
        let projector: Vec<OsString> =
            ["--mmproj", "/m/p.gguf"].iter().map(OsString::from).collect();
        let spec = Spec {
            spec_type: Some("draft-mtp".into()),
            draft: Some(PathBuf::from("/m/d.gguf")),
        };
        let args = engine_args(
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            (8192, "127.0.0.1", 18080),
            &projector,
            &spec,
            &["-ngl".into(), "999".into()],
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        let tail = &args[args.len() - 8..];
        assert_eq!(
            tail,
            ["--mmproj", "/m/p.gguf", "--spec-type", "draft-mtp", "--spec-draft-model", "/m/d.gguf", "-ngl", "999"]
        );
        // `spec_type` alone (the embedded MTP): no draft flag.
        let spec = Spec {
            spec_type: Some("draft-mtp".into()),
            draft: None,
        };
        let args = engine_args(
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            (8192, "127.0.0.1", 18080),
            &[],
            &spec,
            &[],
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|w| w == ["--spec-type", "draft-mtp"]));
        assert!(!args.contains(&"--spec-draft-model"));
    }

    #[test]
    fn the_speculative_settings_resolve() {
        let models = std::env::temp_dir().join(format!("agentLIBRE-spec-{}", std::process::id()));
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("d.gguf"), "").unwrap();
        let settings = |spec_type: Option<&str>, draft: Option<&str>| ServerFile {
            spec_type: spec_type.map(str::to_owned),
            draft: draft.map(str::to_owned),
            ..ServerFile::default()
        };
        // Neither key: no speculative decoding.
        assert_eq!(spec(&settings(None, None), &models).unwrap(), Spec::default());
        // `spec_type` alone: the embedded MTP.
        let got = spec(&settings(Some("draft-mtp"), None), &models).unwrap();
        assert_eq!(got.spec_type.as_deref(), Some("draft-mtp"));
        assert!(got.draft.is_none());
        // Both: the draft is resolved against the models directory.
        let got = spec(&settings(Some("draft-mtp"), Some("d.gguf")), &models).unwrap();
        assert_eq!(got.draft.as_deref(), Some(models.join("d.gguf").as_path()));
        // A `draft` that names no file.
        assert!(matches!(
            spec(&settings(Some("draft-mtp"), Some("nope.gguf")), &models),
            Err(Error::DraftNotFound { path }) if path == models.join("nope.gguf")
        ));
        // A `draft` without a `spec_type`: the build's default is `none`,
        // the draft would not run (AGL-025, D4).
        assert!(matches!(
            spec(&settings(None, Some("d.gguf")), &models),
            Err(Error::MissingSpecType { draft }) if draft == "d.gguf"
        ));
    }

    /// A free port for a test: bound, then closed.
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    /// A unique tag per call, so the parallel tests do not share a dir.
    static FAKE_ENGINE_SETUP: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// A `fake-engine` script in a temp `bin`, a model file in a temp
    /// `models` (AGL-024 tests).
    fn fake_engine_setup() -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let tag = FAKE_ENGINE_SETUP.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "agentLIBRE-engine-{}-{}",
            std::process::id(),
            tag
        ));
        let bin = dir.join("bin");
        let models = dir.join("models");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&models).unwrap();
        let engine = bin.join("fake-engine");
        fs::write(&engine, "#!/bin/sh\nexec sleep 600\n").unwrap();
        fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(models.join("m.gguf"), "").unwrap();
        (dir, bin, models)
    }

    #[test]
    fn launch_with_a_script_engine_spawns_and_shut_down_ends_it() {
        let (dir, bin, models) = fake_engine_setup();
        let url = format!("http://127.0.0.1:{}", free_port());
        let settings = ServerFile {
            llama_url: Some(url.clone()),
            model: Some("m.gguf".into()),
            engine: Some("fake-engine".into()),
            ..ServerFile::default()
        };
        let log = dir.join("store").join("server.log");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut engine = Engine::launch(
            &settings,
            &url,
            &dir,
            &dir,
            &models,
            Some(bin.into_os_string()),
            &log,
        )
        .unwrap();
        let pid = engine.pid();
        assert!(engine.is_alive());
        engine.shutdown();
        // The child was reaped: its pid is gone.
        assert!(!process_alive(pid));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Whether `pid` names a running process the caller may signal.
    fn process_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[test]
    fn launch_refuses_a_busy_port() {
        let (dir, bin, models) = fake_engine_setup();
        // A foreign process answers on the port.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let settings = ServerFile {
            llama_url: Some(url.clone()),
            model: Some("m.gguf".into()),
            engine: Some("fake-engine".into()),
            ..ServerFile::default()
        };
        let result = Engine::launch(
            &settings,
            &url,
            &dir,
            &dir,
            &models,
            Some(bin.into_os_string()),
            &dir.join("server.log"),
        );
        drop(listener);
        fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(
            result,
            Err(Error::PortBusy { url: busy, .. }) if busy == url
        ));
    }

    use crate::forge::Workspace;

    /// A project registered under `origin`, with its own `[server]`.
    fn project(origin: Option<&str>, server: ServerFile) -> Workspace {
        Workspace {
            project: PathBuf::from("/p/agl-lite"),
            identity: "sha256:aa".into(),
            key: "agl-lite-60af300abea6".into(),
            dir: PathBuf::from("/d/agl-lite-60af300abea6"),
            function: None,
            server,
            whisper: Default::default(),
            search: Default::default(),
            origin: origin.map(str::to_owned),
        }
    }

    fn origin(hex12: &str) -> String {
        format!("sha256:{hex12}{}", "0".repeat(52))
    }

    #[test]
    fn the_unit_is_named_by_the_project_and_the_settings_merge() {
        let user = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            model: Some("big.gguf".into()),
            context: Some(131072),
            args: Some(vec!["-ngl".into(), "999".into()]),
            ..ServerFile::default()
        };
        let own = ServerFile {
            llama_url: Some("http://127.0.0.1:18081".into()),
            context: Some(65536),
            ..ServerFile::default()
        };
        let origin = origin("60af300abea6");
        let ws = project(Some(&origin), own);
        let merged = ws.server.over(&user);
        let server =
            Server::of(&ws, &merged, Path::new("/c/agentLIBRE.toml"), None).unwrap();
        assert_eq!(server.unit, "agentLIBRE-llama-server-60af300abea6");
        assert_eq!(server.whisper_unit, "agentLIBRE-whisper-server-60af300abea6");
        assert_eq!(server.url, "http://127.0.0.1:18081");
        assert_eq!(server.settings.model.as_deref(), Some("big.gguf"));
        assert_eq!(server.settings.context, Some(65536));
        assert_eq!(server.project_path, PathBuf::from("/d/agl-lite-60af300abea6/agentLIBRE.toml"));
    }

    #[test]
    fn outside_a_registered_project_there_is_no_server() {
        let user = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            ..ServerFile::default()
        };
        let ws = project(None, ServerFile::default());
        assert!(matches!(
            Server::of(&ws, &user, Path::new("/c/agentLIBRE.toml"), None),
            Err(Error::ProjectNotRegistered { project }) if project == Path::new("/p/agl-lite")
        ));
        // A registered project with no url anywhere is the url error.
        let origin = origin("60af300abea6");
        let ws = project(Some(&origin), ServerFile::default());
        assert!(matches!(
            Server::of(&ws, &ServerFile::default(), Path::new("/c/agentLIBRE.toml"), None),
            Err(Error::MissingServerUrl { .. })
        ));
    }

    #[test]
    fn systemctl_listing_gives_the_unit_names() {
        let listing = "agentLIBRE-llama-server-60af300abea6.service loaded active running llama\n\
                       agentLIBRE-llama-server-0123456789ab.service loaded active running llama\n";
        assert_eq!(
            parse_units(listing),
            ["agentLIBRE-llama-server-60af300abea6", "agentLIBRE-llama-server-0123456789ab"]
        );
        assert!(parse_units("").is_empty());
    }

    #[test]
    fn the_consent_agrees_asks_or_refuses() {
        let question = "the running server does not serve this profile. Restart it? [y/N]";
        // `--yes` and the skip agree without a question.
        assert!(consent(question, &Others::Yes, Lang::En).is_ok());
        assert!(consent(question, &Others::Skip, Lang::En).is_ok());
        // The human answers.
        let yes = |question: &str, lang: Lang| question == question && lang == Lang::En;
        assert!(consent(question, &Others::Ask(&yes), Lang::En).is_ok());
        let no = |_question: &str, _lang: Lang| false;
        assert!(matches!(
            consent(question, &Others::Ask(&no), Lang::En),
            Err(Error::StartDeclined)
        ));
        // No terminal and no `--yes`: refused, no units named (AGL-033, D2).
        assert!(matches!(
            consent(question, &Others::Refuse, Lang::En),
            Err(Error::ServerRunning { units }) if units.is_empty()
        ));
        // The restart consent: the refusal is the `RestartDeclined`.
        assert!(restart_consent("llama-server", &Others::Refuse, Lang::En)
            .is_err_and(|error| matches!(error, Error::RestartDeclined)));
        assert!(restart_consent("llama-server", &Others::Yes, Lang::En).is_ok());
    }

    #[test]
    fn the_answer_is_yes_only_for_y_and_yes() {
        for answer in ["y", "Y\n", " yes ", "YES"] {
            assert!(is_yes(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n", "no", "yep", "ye"] {
            assert!(!is_yes(answer), "{answer:?}");
        }
    }

    #[test]
    fn an_open_port_is_seen() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_open("127.0.0.1", port));
        drop(listener);
        assert!(!port_open("127.0.0.1", port));
    }

    /// A server on a free port that answers every request to `/health`
    /// with the next of `statuses` (the last one repeats); returns its
    /// URL.
    fn health_server(statuses: &'static [u16]) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                let status = statuses[index.min(statuses.len() - 1)];
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        url
    }

    /// A server on a free port (AGL-033, D2): `/health` answers with the
    /// next of `statuses` (the last one repeats), `/v1/models` names the
    /// model `model`, when it names one (else an empty list); returns its
    /// URL.
    fn model_server(statuses: &'static [u16], model: Option<&'static str>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                let request = String::from_utf8_lossy(&buffer);
                if request.starts_with("GET /v1/models") {
                    let body = match model {
                        Some(model) => format!(r#"{{"data":[{{"id":"{model}"}}]}}"#),
                        None => r#"{"data":[]}"#.to_owned(),
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                } else {
                    let status = statuses[index.min(statuses.len() - 1)];
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
            }
        });
        url
    }

    fn autostart<'a>(
        llama: &'a Llama,
        project: &'a Workspace,
        effective: &'a ServerFile,
    ) -> Autostart<'a> {
        static NO_WHISPER: crate::settings::WhisperFile = crate::settings::WhisperFile {
            url: None,
            model: None,
            args: None,
        };
        Autostart {
            llama,
            project,
            effective,
            whisper: &NO_WHISPER,
            user_path: PathBuf::from("/c/agentLIBRE.toml"),
            models: PathBuf::from("/m"),
            path_var: None,
            lang: Lang::En,
            others: Others::Refuse,
            engine: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn a_server_that_answers_is_not_started_again() {
        let url = health_server(&[200]);
        let llama = Llama::new(&url);
        // Not even a registered project is needed.
        let ws = project(None, ServerFile::default());
        let settings = ServerFile::default();
        let mut said = Vec::new();
        autostart(&llama, &ws, &settings)
            .ensure(&mut |line| said.push(line.to_owned()))
            .unwrap();
        assert!(said.is_empty());
    }

    #[test]
    fn a_server_that_loads_is_waited_for() {
        let url = health_server(&[503, 503, 200]);
        let llama = Llama::new(&url);
        let ws = project(None, ServerFile::default());
        let settings = ServerFile::default();
        let mut said = Vec::new();
        autostart(&llama, &ws, &settings)
            .ensure(&mut |line| said.push(line.to_owned()))
            .unwrap();
        assert_eq!(
            said,
            [format!("the server is ready at {url}: llama-server")]
        );
    }

    #[test]
    fn outside_a_registered_project_the_launch_proceeds_and_fails_on_the_model() {
        // Nothing listens here: the port was just closed, so the auto path
        // launches (it needs no unit, AGL-024) and fails on the model file.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let llama = Llama::new(&url);
        let ws = project(None, ServerFile::default());
        let settings = ServerFile {
            llama_url: Some(url),
            model: Some("m.gguf".into()),
            ..ServerFile::default()
        };
        let mut said = Vec::new();
        let result = autostart(&llama, &ws, &settings)
            .ensure(&mut |line| said.push(line.to_owned()));
        assert!(matches!(
            result,
            Err(Error::ModelNotFound { path }) if path == Path::new("/m/m.gguf")
        ));
        assert!(said.is_empty());
    }

    #[test]
    fn the_model_check_of_the_running_server() {
        // The check of D2 (AGL-033): the expected model is the full
        // path, as the llama-server names it; the observed one is from
        // `/v1/models`.
        let models = Path::new("/m");
        let settings = ServerFile {
            model: Some("m.gguf".into()),
            ..ServerFile::default()
        };
        // The same model: it serves.
        let url = model_server(&[200], Some("/m/m.gguf"));
        let llama = Llama::new(&url);
        assert!(serves(&llama, None, &settings, models).unwrap());
        // Another model: it does not serve.
        let url = model_server(&[200], Some("/m/other.gguf"));
        let llama = Llama::new(&url);
        assert!(!serves(&llama, None, &settings, models).unwrap());
        // The server answers but does not name a model: nothing to
        // compare, no restart.
        let url = model_server(&[200], None);
        let llama = Llama::new(&url);
        assert!(serves(&llama, None, &settings, models).unwrap());
    }

    #[test]
    fn the_record_decides_what_the_running_server_was_launched_with() {
        // AGL-035, D1: the effective settings are compared with the record
        // of the launch, not with the settings file.
        let models = Path::new("/m");
        let profile = ServerFile {
            model: Some("m.gguf".into()),
            context: Some(16384),
            ..ServerFile::default()
        };
        let url = model_server(&[200], Some("/m/m.gguf"));
        let llama = Llama::new(&url);
        // The server runs exactly the profile: it serves.
        assert!(serves(&llama, Some(&profile), &profile, models).unwrap());
        // A key that differs from the record: it does not serve, though
        // the observed model is the same.
        let wider = ServerFile {
            context: Some(131072),
            ..profile.clone()
        };
        assert!(!serves(&llama, Some(&profile), &wider, models).unwrap());
        // Without a record the model alone decides.
        assert!(serves(&llama, None, &wider, models).unwrap());
    }

    #[test]
    fn the_record_is_written_read_and_removed() {
        let dir = std::env::temp_dir().join(format!("agentLIBRE-record-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let effective = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            model: Some("m.gguf".into()),
            context: Some(16384),
            args: Some(vec!["-ngl".into(), "999".into()]),
            ..ServerFile::default()
        };
        assert_eq!(read_record(&dir), None);
        // The record of a process that runs reads back as written.
        write_record(&dir, std::process::id(), &effective).unwrap();
        assert_eq!(read_record(&dir), Some(effective.clone()));
        // The record of a process that is gone is no record.
        write_record(&dir, u32::MAX, &effective).unwrap();
        assert_eq!(read_record(&dir), None);
        // A record that cannot be read is no record.
        fs::write(dir.join(STATE_FILE), "pid = \"x\"\n").unwrap();
        assert_eq!(read_record(&dir), None);
        write_record(&dir, std::process::id(), &effective).unwrap();
        remove_record(&dir);
        assert_eq!(read_record(&dir), None);
        assert!(!dir.join(STATE_FILE).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A local url where nothing listens: a port that was free a moment ago.
    fn free_url() -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    #[test]
    fn the_owned_server_of_the_other_kind_is_told_by_the_record() {
        // AGL-036, D10: the record says where the owned server runs; a url
        // other than the effective kind's is the other kind's server.
        let dir = std::env::temp_dir().join(format!("agentLIBRE-elsewhere-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let llama = free_url();
        let strata = free_url();
        let record = ServerFile {
            kind: Some("llama".into()),
            llama_url: Some(llama.clone()),
            model: Some("m.gguf".into()),
            ..ServerFile::default()
        };
        // No record and no unit: nothing is owned.
        assert!(owned_elsewhere(&dir, None, &strata).unwrap().is_none());
        write_record(&dir, std::process::id(), &record).unwrap();
        // The same url: the server of the kind, not elsewhere.
        assert!(owned_elsewhere(&dir, None, &llama).unwrap().is_none());
        let elsewhere = owned_elsewhere(&dir, None, &strata).unwrap().unwrap();
        assert_eq!(elsewhere.url.as_deref(), Some(llama.as_str()));
        assert_eq!(elsewhere.what.as_deref(), Some("llama-server (m.gguf)"));
        // The switch refused: the record stays, the error names the switch.
        let mut said = Vec::new();
        let refused = switch_kind(
            &dir,
            None,
            None,
            (&strata, "the Strata server"),
            &Others::Refuse,
            &mut |line| said.push(line.to_owned()),
            Lang::En,
        );
        assert!(matches!(refused, Err(Error::SwitchDeclined)));
        assert!(read_record(&dir).is_some());
        // Agreed: the owned server stops, the record goes, and it is told.
        switch_kind(
            &dir,
            None,
            None,
            (&strata, "the Strata server"),
            &Others::Yes,
            &mut |line| said.push(line.to_owned()),
            Lang::En,
        )
        .unwrap();
        assert!(read_record(&dir).is_none());
        assert_eq!(said, ["llama-server (m.gguf) stopped"]);
        // Nothing owned elsewhere: no question, nothing said.
        let mut silent = Vec::new();
        switch_kind(
            &dir,
            None,
            None,
            (&strata, "the Strata server"),
            &Others::Refuse,
            &mut |line| silent.push(line.to_owned()),
            Lang::En,
        )
        .unwrap();
        assert!(silent.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_server_that_serves_the_expected_model_is_left_alone() {
        // The running server serves the model the settings name (AGL-033,
        // D2): `ensure` says nothing and does not touch it.
        let url = model_server(&[200], Some("/m/m.gguf"));
        let llama = Llama::new(&url);
        let ws = project(None, ServerFile::default());
        let settings = ServerFile {
            llama_url: Some(url),
            model: Some("m.gguf".into()),
            ..ServerFile::default()
        };
        let mut said = Vec::new();
        autostart(&llama, &ws, &settings)
            .ensure(&mut |line| said.push(line.to_owned()))
            .unwrap();
        assert!(said.is_empty());
    }

    #[test]
    fn a_server_that_serves_another_model_is_restarted() {
        // The running server serves another model (AGL-033, D2): the
        // restart wants the consent; without it (the `Refuse` of the
        // non-interactive run) it declines.
        let url = model_server(&[200], Some("/m/other.gguf"));
        let llama = Llama::new(&url);
        let ws = project(None, ServerFile::default());
        let settings = ServerFile {
            llama_url: Some(url),
            model: Some("m.gguf".into()),
            ..ServerFile::default()
        };
        let mut said = Vec::new();
        assert!(matches!(
            autostart(&llama, &ws, &settings)
                .ensure(&mut |line| said.push(line.to_owned())),
            Err(Error::RestartDeclined)
        ));
        assert!(said.is_empty());
    }

    #[test]
    fn the_strata_arguments_name_the_python_the_script_the_config_and_the_port() {
        // The launch of the Strata server (AGL-033, D2): the venv python
        // runs `serve/server.py` with the config and the port; the
        // libraries and the environment the config names are applied by
        // the server itself.
        let launch = StrataLaunch {
            python: PathBuf::from("/r/.venv/bin/python"),
            server_py: PathBuf::from("/r/serve/server.py"),
            config: PathBuf::from("/r/strata.json"),
            port: 18088,
            cwd: Some(PathBuf::from("/w")),
        };
        let args: Vec<String> = strata_args(&launch)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "/r/.venv/bin/python",
                "/r/serve/server.py",
                "--engine",
                "strata",
                "--config",
                "/r/strata.json",
                "--port",
                "18088",
            ]
        );
    }

    #[test]
    fn the_systemd_run_arguments_carry_the_unit_the_cwd_and_the_server() {
        let launch = StrataLaunch {
            python: PathBuf::from("/r/.venv/bin/python"),
            server_py: PathBuf::from("/r/serve/server.py"),
            config: PathBuf::from("/r/strata.json"),
            port: 18088,
            cwd: Some(PathBuf::from("/w")),
        };
        let args: Vec<String> = strata_systemd_run_args(
            "agentLIBRE-llama-server-60af300abea6",
            &launch,
        )
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
        assert_eq!(
            args,
            [
                "--user",
                "--unit=agentLIBRE-llama-server-60af300abea6",
                "--collect",
                "--quiet",
                "--property=WorkingDirectory=/w",
                "--",
                "/r/.venv/bin/python",
                "/r/serve/server.py",
                "--engine",
                "strata",
                "--config",
                "/r/strata.json",
                "--port",
                "18088",
            ]
        );
        // Without the cwd: no property, the rest is the same.
        let launch = StrataLaunch {
            python: PathBuf::from("/r/.venv/bin/python"),
            server_py: PathBuf::from("/r/serve/server.py"),
            config: PathBuf::from("/r/strata.json"),
            port: 18088,
            cwd: None,
        };
        let args: Vec<String> = strata_systemd_run_args(
            "agentLIBRE-llama-server-60af300abea6",
            &launch,
        )
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
        assert!(!args.iter().any(|arg| arg.starts_with("--property")));
        assert_eq!(args[4], "--");
    }

    #[test]
    fn the_function_table_is_the_effective_settings() {
        // The `[server]` table of the function file is the kind and the
        // model side (AGL-033, D1; AGL-036, D1): self-contained — a table
        // without `kind` is llama (D5) —, while the url of the kind and
        // `engine` stay of the settings.
        let origin = origin("60af300abea6");
        let own = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            model: Some("project.gguf".into()),
            ..ServerFile::default()
        };
        let user = ServerFile {
            context: Some(131072),
            ..ServerFile::default()
        };
        let ws = project(Some(&origin), own);
        let merged = ws.server.over(&user);
        let table = ServerFile {
            model: Some("function.gguf".into()),
            context: Some(4096),
            ..ServerFile::default()
        };
        let server = Server::of(
            &ws,
            &merged,
            Path::new("/c/agentLIBRE.toml"),
            Some((
                table.clone(),
                Path::new("/f/function.toml").to_path_buf(),
            )),
        )
        .unwrap();
        assert_eq!(
            server.effective_settings().unwrap(),
            ServerFile {
                kind: Some("llama".into()),
                llama_url: Some("http://127.0.0.1:18080".into()),
                model: Some("function.gguf".into()),
                context: Some(4096),
                ..ServerFile::default()
            }
        );
        // Without the table, the resolved settings are the effective.
        let server = Server::of(&ws, &merged, Path::new("/c/agentLIBRE.toml"), None)
            .unwrap();
        assert_eq!(server.effective_settings().unwrap(), merged);
    }
}
