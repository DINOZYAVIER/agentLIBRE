//! `agl-bridge --listen <address:port> --dir <directory> --socket <path>`
//! (AGL-083, D18): the bridge, as `agl bridge start` runs it in its
//! systemd unit. It reads no settings: everything comes as arguments.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;

use agentlibre_bridge::{Config, Store, serve};

const USAGE: &str = "usage: agl-bridge --listen <address:port> --dir <directory> --socket <path>";

fn main() -> ExitCode {
    let config = match parse(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("agl-bridge: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match serve(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agl-bridge: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Config, String> {
    let (mut listen, mut dir, mut socket) = (None, None, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--listen" => &mut listen,
            "--dir" => &mut dir,
            "--socket" => &mut socket,
            _ => return Err(format!("an unknown argument: {arg}")),
        };
        if slot.is_some() {
            return Err(format!("{arg} twice"));
        }
        *slot = Some(args.next().ok_or_else(|| format!("{arg} needs a value"))?);
    }
    let listen = listen.ok_or("--listen is missing")?;
    let listen: SocketAddr = listen
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
        .ok_or_else(|| format!("{listen} is no address and port"))?;
    Ok(Config {
        listen,
        store: Store::new(PathBuf::from(dir.ok_or("--dir is missing")?)),
        socket: PathBuf::from(socket.ok_or("--socket is missing")?),
    })
}
