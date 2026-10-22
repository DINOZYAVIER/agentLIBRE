//! `agl server start|stop|status` and the autostart: the project's
//! llama-server as a transient systemd user unit (D146-D150, AGL-015),
//! or, on the agent command's path, the inference engine as a background
//! child of agl (AGL-024).

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::llama::Llama;
use crate::cli::args::ServerAction;
use crate::error::Error;
use crate::forge::Workspace;
use crate::i18n::{Lang, Msg, m, server as texts};
use crate::settings::ServerFile;
use crate::tr;

/// The name every unit of a llama-server starts with (D8); the project's
/// key follows after a dash.
pub const UNIT_PREFIX: &str = "agentLIBRE-llama-server";

/// `context` when `[server]` does not set it (D148).
const DEFAULT_CONTEXT: u32 = 16384;

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
    /// The project's `[server]` over the user's.
    pub settings: ServerFile,
    pub url: String,
    /// The user's and the project's settings files, for hints.
    pub user_path: PathBuf,
    pub project_path: PathBuf,
}

impl Server {
    /// The server of `project`, with `settings` the project's `[server]`
    /// over the user's. A project that is not registered has no unit: an
    /// error (D14).
    pub fn of(
        project: &Workspace,
        settings: &ServerFile,
        user_path: &Path,
    ) -> Result<Server, Error> {
        let key = project.unit_key().ok_or_else(|| Error::ProjectNotRegistered {
            project: project.project.clone(),
        })?;
        let project_path = project.dir.join(crate::forge::MARKER_FILE);
        let url = settings.url.clone().ok_or_else(|| Error::MissingServerUrl {
            user_path: user_path.into(),
            project_path: project_path.clone(),
        })?;
        Ok(Server {
            unit: unit_name(&key),
            whisper_unit: format!("{}-{key}", super::whisper::UNIT_PREFIX),
            settings: settings.clone(),
            url,
            user_path: user_path.into(),
            project_path,
        })
    }
}

/// The unit of the project whose unit key is `key` (D8).
pub fn unit_name(key: &str) -> String {
    format!("{UNIT_PREFIX}-{key}")
}

/// What `start` does when the server of another project runs (D9, D13).
pub enum Others<'a> {
    /// Ask the human; `true` starts.
    Ask(&'a dyn Fn(&[String]) -> bool),
    /// `--yes`: start without asking.
    Yes,
    /// Nobody can answer and there is no `--yes`: refuse.
    Refuse,
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
            let ask = |units: &[String]| ask_terminal(units, lang);
            let others = if yes {
                Others::Yes
            } else if crate::repl::term::is_interactive() {
                Others::Ask(&ask)
            } else {
                Others::Refuse
            };
            start(
                server,
                models,
                path_var,
                &llama,
                &others,
                &mut |line| println!("{line}"),
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
                println!("{}", tr!(lang, m::SERVER_STOPPED));
            } else if !whisper {
                println!("{}", texts::not_running(&server.unit, lang));
            }
            if whisper {
                println!("{}", tr!(lang, m::WHISPER_STOPPED));
            }
            Ok(ExitCode::SUCCESS)
        }
        ServerAction::Status => {
            let active = unit_active(&server.unit)?;
            let health = health(&llama);
            let model = server.settings.model.as_deref();
            println!(
                "{}",
                texts::status(&server.unit, active, &server.url, model, health, lang)
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
/// (D146, D149); the same code as the autostart (AGL-015, D10).
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
    if unit_active(&server.unit)? {
        say(&tr!(lang, m::SERVER_ALREADY_RUNNING, url = url));
        return Ok(());
    }
    let (host, port) =
        local_address(url).ok_or_else(|| Error::NotLocalServer { url: url.into() })?;
    let running = active_units()?;
    // The port is held by something that is not this project's unit
    // (D7): the units that run are named, there may be none.
    if health(llama) != Health::Down || port_open(&host, port) {
        return Err(Error::PortBusy {
            url: url.into(),
            units: running,
        });
    }
    consent(running, others)?;
    let model = server
        .settings
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
    let program = find_program(DEFAULT_ENGINE, path_var).ok_or_else(|| Error::EngineNotFound {
        engine: DEFAULT_ENGINE.to_owned(),
    })?;
    let context = server.settings.context.unwrap_or(DEFAULT_CONTEXT);
    let extra = server.settings.args.clone().unwrap_or_default();
    let projector = projector_args(&server.settings, models)?;
    let args = systemd_run_args(
        &server.unit,
        &program,
        &model,
        (context, &host, port),
        &projector,
        &extra,
    );
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
    let name = model.file_name().unwrap_or_default().to_string_lossy();
    say(&texts::starting(&name, url, &server.unit, lang));
    wait_ready(llama, Some(&server.unit), &m::SERVER_READY, say, lang)
}

/// Waits until `/health` answers ready (D149). `unit` is the unit that
/// loads the model, when it is ours: it stopping is a failure.
pub(super) fn wait_ready(
    llama: &Llama,
    unit: Option<&str>,
    ready: &Msg,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let started = Instant::now();
    loop {
        if health(llama) == Health::Ready {
            say(&tr!(lang, ready, url = llama.url()));
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
/// D6, D10); the same code as `agl server start`.
pub struct Autostart<'a> {
    pub llama: &'a Llama,
    pub project: &'a Workspace,
    /// The project's `[server]` over the user's.
    pub settings: &'a ServerFile,
    /// The project's `[whisper]` over the user's (AGL-014, D22).
    pub whisper: &'a crate::settings::WhisperFile,
    pub user_path: PathBuf,
    pub models: PathBuf,
    pub path_var: Option<OsString>,
    pub lang: Lang,
    /// The engine child agl launched itself (AGL-024, D2, D3): `None`
    /// while agl uses a server it does not own.
    pub engine: Arc<Mutex<Option<Engine>>>,
}

impl Autostart<'_> {
    /// Returns when the server answers: it did, it was loading and is
    /// ready now, or agl launched it itself as a background child
    /// (AGL-024, D2). `say` takes the progress lines.
    pub fn ensure(&self, say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        let llama = self.llama;
        match health(llama) {
            Health::Ready => Ok(()),
            // Someone loads it now: wait for it, as `start` does.
            Health::Loading => wait_ready(llama, None, &m::SERVER_READY, say, self.lang),
            // Connection refused: launch the engine as a background child
            // (AGL-024, D2); a foreign process on the port is an error,
            // not a target.
            Health::Down => {
                let log = self.project.dir.join("server.log");
                let engine = Engine::launch(
                    self.settings,
                    llama.url(),
                    &self.user_path,
                    &self.project.dir.join(crate::forge::MARKER_FILE),
                    &self.models,
                    self.path_var.clone(),
                    &log,
                )?;
                let mut guard = self.engine.lock().unwrap();
                *guard = Some(engine);
                let engine = guard.as_mut().expect("the engine just launched");
                wait_child_ready(llama, engine, &log, say, self.lang)
            }
        }
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
            engine_args(&program, &model, (context, &host, port), &projector, &extra);
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

/// Waits until the engine agl launched answers ready (AGL-024, D2): the
/// same timeout and polling as `wait_ready`; the engine's own output is
/// in the log file the errors point at.
fn wait_child_ready(
    llama: &Llama,
    engine: &mut Engine,
    log: &Path,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let started = Instant::now();
    loop {
        if health(llama) == Health::Ready {
            say(&tr!(lang, m::SERVER_READY, url = llama.url()));
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

/// The server of another project runs: ask, or refuse (D9, D13). With
/// no other unit nothing is asked.
fn consent(running: Vec<String>, others: &Others) -> Result<(), Error> {
    if running.is_empty() {
        return Ok(());
    }
    match others {
        Others::Yes => Ok(()),
        Others::Ask(ask) if ask(&running) => Ok(()),
        Others::Ask(_) => Err(Error::StartDeclined),
        Others::Refuse => Err(Error::ServerRunning { units: running }),
    }
}

/// The question of D9 on a terminal: "a server is running, start one
/// more?"; the answer is a line of stdin, `y` or `yes` starts.
fn ask_terminal(units: &[String], lang: Lang) -> bool {
    print!("{} ", texts::others_running(units, lang));
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

pub(super) fn health(llama: &Llama) -> Health {
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
/// the extra of `[server] args` (AGL-024, D4): the one source of the
/// launch arguments, the systemd and the child paths alike.
pub(super) fn engine_args(
    engine: &Path,
    model: &Path,
    (context, host, port): (u32, &str, u16),
    projector: &[OsString],
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
        engine, model, (context, host, port), projector, extra,
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
                Health::Loading,
                Lang::En
            ),
            "unit: agentLIBRE-llama-server-60af300abea6 (active)\nurl: http://127.0.0.1:18080\n\
             model: not set\nhealth: loading"
        );
        assert_eq!(
            texts::status(
                "agentLIBRE-llama-server-60af300abea6",
                false,
                "http://x",
                Some("q.gguf"),
                Health::Down,
                Lang::Uk
            ),
            "unit: agentLIBRE-llama-server-60af300abea6 (неактивний)\nадреса: http://x\n\
             модель: q.gguf\nстан: не відповідає"
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
            &["-ngl".into(), "999".into()],
        );
        let engine = engine_args(
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            (8192, "127.0.0.1", 18080),
            &[],
            &["-ngl".into(), "999".into()],
        );
        // The systemd prefix is `--user --unit=… --collect --quiet --`;
        // the engine arguments are the rest (AGL-024, D4).
        assert_eq!(systemd.len(), 5 + engine.len());
        let tail: Vec<&OsString> = systemd.iter().skip(5).collect();
        let engine: Vec<&OsString> = engine.iter().collect();
        assert_eq!(tail, engine);
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
            url: Some(url.clone()),
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
            url: Some(url.clone()),
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
            url: Some("http://127.0.0.1:18080".into()),
            model: Some("big.gguf".into()),
            context: Some(131072),
            args: Some(vec!["-ngl".into(), "999".into()]),
            ..ServerFile::default()
        };
        let own = ServerFile {
            url: Some("http://127.0.0.1:18081".into()),
            context: Some(65536),
            ..ServerFile::default()
        };
        let origin = origin("60af300abea6");
        let ws = project(Some(&origin), own);
        let merged = ws.server.over(&user);
        let server = Server::of(&ws, &merged, Path::new("/c/agentLIBRE.toml")).unwrap();
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
            url: Some("http://127.0.0.1:18080".into()),
            ..ServerFile::default()
        };
        let ws = project(None, ServerFile::default());
        assert!(matches!(
            Server::of(&ws, &user, Path::new("/c/agentLIBRE.toml")),
            Err(Error::ProjectNotRegistered { project }) if project == Path::new("/p/agl-lite")
        ));
        // A registered project with no url anywhere is the url error.
        let origin = origin("60af300abea6");
        let ws = project(Some(&origin), ServerFile::default());
        assert!(matches!(
            Server::of(&ws, &ServerFile::default(), Path::new("/c/agentLIBRE.toml")),
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
    fn another_running_server_is_asked_about_or_refused() {
        let running = || vec!["agentLIBRE-llama-server-0123456789ab".to_owned()];
        // Nothing runs: nobody is asked.
        let never = |_: &[String]| -> bool { panic!("nothing to ask") };
        assert!(consent(Vec::new(), &Others::Ask(&never)).is_ok());
        assert!(consent(Vec::new(), &Others::Refuse).is_ok());
        // `--yes`.
        assert!(consent(running(), &Others::Yes).is_ok());
        // The human answers.
        let yes = |units: &[String]| units == ["agentLIBRE-llama-server-0123456789ab"];
        assert!(consent(running(), &Others::Ask(&yes)).is_ok());
        let no = |_: &[String]| false;
        assert!(matches!(
            consent(running(), &Others::Ask(&no)),
            Err(Error::StartDeclined)
        ));
        // `-p` and stdin without `--yes`: refused, with the unit named.
        assert!(matches!(
            consent(running(), &Others::Refuse),
            Err(Error::ServerRunning { units }) if units == running()
        ));
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

    fn autostart<'a>(
        llama: &'a Llama,
        project: &'a Workspace,
        settings: &'a ServerFile,
    ) -> Autostart<'a> {
        static NO_WHISPER: crate::settings::WhisperFile = crate::settings::WhisperFile {
            url: None,
            model: None,
            args: None,
        };
        Autostart {
            llama,
            project,
            settings,
            whisper: &NO_WHISPER,
            user_path: PathBuf::from("/c/agentLIBRE.toml"),
            models: PathBuf::from("/m"),
            path_var: None,
            lang: Lang::En,
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
        assert_eq!(said, [format!("llama-server is ready at {url}")]);
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
            url: Some(url),
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
}
