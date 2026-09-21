use std::path::PathBuf;

use agl_execd::{ExecutionServer, ListenerSource, default_socket_path};
use anyhow::{Context, Result};

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let roots = roots()?;
    let launcher = sibling_launcher()?;
    let server = ExecutionServer::start(&roots.0.join("execd"), launcher)?;
    let source = if std::env::var_os("LISTEN_FDS").is_some() {
        ListenerSource::Systemd
    } else {
        ListenerSource::Bind(default_socket_path(&roots.1))
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(server.serve(source))
}

fn roots() -> Result<(PathBuf, PathBuf)> {
    if let Some(root) = std::env::var_os("AGL_HOME") {
        let root = PathBuf::from(root);
        anyhow::ensure!(root.is_absolute(), "AGL_HOME must be absolute");
        return Ok((root.join("data"), root.join("state")));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|path| path.join(".local/share")))
        .context("HOME or XDG_DATA_HOME is required")?
        .join("agentLIBRE");
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|path| path.join(".local/state")))
        .context("HOME or XDG_STATE_HOME is required")?
        .join("agentLIBRE");
    Ok((data, state))
}

fn sibling_launcher() -> Result<PathBuf> {
    let current = std::env::current_exe()?.canonicalize()?;
    let bin = current
        .parent()
        .context("agl-execd executable has no parent")?;
    let sibling = bin.join("agl-execd-launcher");
    if sibling.is_file() {
        return Ok(sibling);
    }
    let installed = bin
        .parent()
        .map(|prefix| prefix.join("libexec/agentlibre/agl-execd-launcher"))
        .context("agl-execd executable has no installation prefix")?;
    anyhow::ensure!(installed.is_file(), "agl-execd launcher is not installed");
    Ok(installed)
}
