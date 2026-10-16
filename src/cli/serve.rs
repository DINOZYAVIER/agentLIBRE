//! `agentLIBRE server start|stop|status`: llama-server as a transient systemd user
//! unit (D146-D150).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::llama::Llama;
use crate::cli::args::ServerAction;
use crate::error::Error;
use crate::i18n::{Lang, server as texts};
use crate::settings::UserFile;

/// The systemd user unit (D147).
pub const UNIT: &str = "agentLIBRE-llama-server";

/// `context` when `[server]` does not set it (D148).
const DEFAULT_CONTEXT: u32 = 16384;

/// How long `start` waits for `/health` (D149).
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(500);

/// Hosts `start` can run llama-server on (D148).
const LOCAL_HOSTS: [&str; 2] = ["127.0.0.1", "localhost"];

/// What `/health` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Ready,
    /// The server answers but is not ready, as while the model loads.
    Loading,
    Down,
}

/// Runs a server command. `models` is the models directory and
/// `user_path` the user settings file, for hints.
pub fn run(
    action: ServerAction,
    user: &UserFile,
    user_path: &Path,
    models: &Path,
    path_var: Option<OsString>,
    lang: Lang,
) -> Result<ExitCode, Error> {
    let url = user
        .server_url
        .clone()
        .ok_or_else(|| Error::MissingServerUrl {
            user_path: user_path.into(),
        })?;
    let llama = Llama::new(&url);
    match action {
        ServerAction::Start => start(user, user_path, models, path_var, &url, &llama, lang),
        ServerAction::Stop => {
            if unit_active()? {
                systemctl(&["stop", UNIT])?;
                println!("{}", texts::stopped(lang));
            } else {
                println!("{}", texts::not_running(lang));
            }
            Ok(ExitCode::SUCCESS)
        }
        ServerAction::Status => {
            let active = unit_active()?;
            let health = health(&llama);
            let model = user.launch.model.as_deref();
            println!("{}", texts::status(active, &url, model, health, lang));
            Ok(if health == Health::Ready {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

fn start(
    user: &UserFile,
    user_path: &Path,
    models: &Path,
    path_var: Option<OsString>,
    url: &str,
    llama: &Llama,
    lang: Lang,
) -> Result<ExitCode, Error> {
    if unit_active()? || health(llama) != Health::Down {
        println!("{}", texts::already_running(url, lang));
        return Ok(ExitCode::SUCCESS);
    }
    let (host, port) =
        local_address(url).ok_or_else(|| Error::NotLocalServer { url: url.into() })?;
    let model = user
        .launch
        .model
        .as_deref()
        .ok_or_else(|| Error::MissingModel {
            user_path: user_path.into(),
            models: models.into(),
        })?;
    let model = models.join(model);
    if !model.is_file() {
        return Err(Error::ModelNotFound { path: model });
    }
    let program = find_program("llama-server", path_var).ok_or(Error::LlamaServerNotFound)?;
    let context = user.launch.context.unwrap_or(DEFAULT_CONTEXT);
    let args = systemd_run_args(&program, &model, context, &host, port, &user.launch.args);
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
    println!("{}", texts::starting(&name, url, lang));
    let started = Instant::now();
    loop {
        if health(llama) == Health::Ready {
            println!("{}", texts::ready(url, lang));
            return Ok(ExitCode::SUCCESS);
        }
        if !unit_active()? {
            return Err(Error::ServerFailed);
        }
        if started.elapsed() >= READY_TIMEOUT {
            return Err(Error::ServerTimeout {
                seconds: READY_TIMEOUT.as_secs(),
            });
        }
        thread::sleep(POLL);
    }
}

fn health(llama: &Llama) -> Health {
    match llama.health() {
        Some(200) => Health::Ready,
        Some(_) => Health::Loading,
        None => Health::Down,
    }
}

/// Whether the unit runs.
fn unit_active() -> Result<bool, Error> {
    let status = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", UNIT])
        .stdin(Stdio::null())
        .status()
        .map_err(|source| Error::Systemd {
            detail: source.to_string(),
        })?;
    Ok(status.success())
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
fn local_address(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80),
    };
    LOCAL_HOSTS.contains(&host).then(|| (host.to_owned(), port))
}

/// The first executable `name` in `path_var` (D147).
fn find_program(name: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The arguments of `systemd-run`: the unit, then llama-server with the
/// arguments agentLIBRE needs and those of `[server] args` (D147, D148).
fn systemd_run_args(
    program: &Path,
    model: &Path,
    context: u32,
    host: &str,
    port: u16,
    extra: &[String],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--user",
        &format!("--unit={UNIT}"),
        "--collect",
        "--quiet",
        "--",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(program.into());
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
    args.extend(extra.iter().map(OsString::from));
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
                true,
                "http://127.0.0.1:18080",
                None,
                Health::Loading,
                Lang::En
            ),
            "unit: agentLIBRE-llama-server (active)\nurl: http://127.0.0.1:18080\nmodel: not set\n\
             health: loading"
        );
        assert_eq!(
            texts::status(false, "http://x", Some("q.gguf"), Health::Down, Lang::Uk),
            "unit: agentLIBRE-llama-server (неактивний)\nадреса: http://x\nмодель: q.gguf\n\
             стан: не відповідає"
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
            Path::new("/bin/llama-server"),
            Path::new("/m/q.gguf"),
            8192,
            "127.0.0.1",
            18080,
            &["-ngl".into(), "999".into()],
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user",
                "--unit=agentLIBRE-llama-server",
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
}
