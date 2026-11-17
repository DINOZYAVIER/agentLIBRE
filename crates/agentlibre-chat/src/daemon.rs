//! The chat's connection to `agl daemon` (AGL-076; AGL-073 D5): its socket,
//! the daemon started when none answers (AGL-076 D6), `hello` and the lines
//! of the protocol.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentlibre_proto::{self as proto, Lines, Reply, Request};

/// How long a daemon that was started may take to answer.
const START_WAIT: Duration = Duration::from_secs(10);

/// The daemon's socket: `$XDG_RUNTIME_DIR/agentLIBRE/agl.sock` (AGL-076 D7).
fn socket() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime)
        .join("agentLIBRE")
        .join(proto::SOCKET))
}

/// A connection, its `hello` said: what writes to it and what reads it.
pub fn connect(lang: &str) -> Result<(UnixStream, Lines<BufReader<UnixStream>>), String> {
    let path = socket()?;
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(_) => start(&path)?,
    };
    let reader = stream.try_clone().map_err(|error| error.to_string())?;
    let mut lines = Lines::new(BufReader::new(reader));
    let mut out = stream;
    proto::send(
        &mut out,
        &Request::Hello {
            version: proto::VERSION,
            // A human answers the planes' questions (D11).
            human: true,
            lang: lang.to_owned(),
            pid: std::process::id(),
        },
    )
    .map_err(|error| error.to_string())?;
    match lines.read::<Reply>() {
        Ok(Some(Reply::Hello { .. })) => Ok((out, lines)),
        Ok(Some(Reply::Refused { error })) => Err(error),
        Ok(other) => Err(format!("the daemon answered {other:?}")),
        Err(error) => Err(error.to_string()),
    }
}

/// Starts `agl daemon` apart from this process and waits until `path`
/// answers.
fn start(path: &PathBuf) -> Result<UnixStream, String> {
    Command::new("agl")
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("agl daemon: {error}"))?;
    let since = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            return Ok(stream);
        }
        if since.elapsed() > START_WAIT {
            return Err(format!("{} does not answer", path.display()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
