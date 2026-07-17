use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListenerSource {
    Bind(PathBuf),
    Systemd,
}

impl Display for ListenerSource {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(path) => write!(formatter, "bind:{}", path.display()),
            Self::Systemd => formatter.write_str("systemd:agentlibre"),
        }
    }
}

#[cfg(unix)]
pub(crate) fn claim_systemd_listener() -> Result<tokio::net::UnixListener> {
    use std::os::fd::FromRawFd as _;

    let pid = std::env::var("LISTEN_PID").context("LISTEN_PID is required")?;
    let descriptor_count = std::env::var("LISTEN_FDS").context("LISTEN_FDS is required")?;
    let names = std::env::var("LISTEN_FDNAMES").context("LISTEN_FDNAMES is required")?;
    validate_activation_environment(&pid, &descriptor_count, &names)?;
    validate_listener_fd(3)?;

    // SAFETY: fd 3 is consumed exactly once after validating systemd's PID,
    // descriptor count/name, socket type, listening state and effective UID.
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    listener
        .set_nonblocking(true)
        .context("failed to set inherited daemon listener nonblocking")?;
    clear_activation_environment();
    tokio::net::UnixListener::from_std(listener)
        .context("failed to adopt inherited systemd listener into Tokio")
}

#[cfg(not(unix))]
pub(crate) fn claim_systemd_listener() -> Result<tokio::net::UnixListener> {
    bail!("systemd socket activation is available only on Unix")
}

fn validate_activation_environment(pid: &str, descriptor_count: &str, names: &str) -> Result<()> {
    let pid = pid
        .parse::<u32>()
        .context("LISTEN_PID is not a valid process ID")?;
    if pid != std::process::id() {
        bail!("LISTEN_PID does not match this daemon process");
    }
    let descriptor_count = descriptor_count
        .parse::<u32>()
        .context("LISTEN_FDS is not a valid descriptor count")?;
    if descriptor_count != 1 {
        bail!("systemd activation requires exactly one inherited descriptor");
    }
    if names != "agentlibre" {
        bail!("LISTEN_FDNAMES must contain exactly `agentlibre`");
    }
    Ok(())
}

#[cfg(unix)]
fn validate_listener_fd(fd: libc::c_int) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to writable initialized storage on success.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("inherited descriptor 3 cannot be inspected");
    }
    // SAFETY: fstat succeeded and initialized the structure.
    let stat = unsafe { stat.assume_init() };
    // SAFETY: geteuid has no preconditions.
    if stat.st_uid != unsafe { libc::geteuid() } {
        bail!("inherited listener is not owned by the daemon effective UID");
    }
    let socket_type = socket_option(fd, libc::SO_TYPE)?;
    if socket_type != libc::SOCK_STREAM {
        bail!("inherited descriptor is not a stream socket");
    }
    if socket_option(fd, libc::SO_ACCEPTCONN)? != 1 {
        bail!("inherited descriptor is not a listening socket");
    }
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: address and length provide valid writable getsockname storage.
    if unsafe {
        libc::getsockname(
            fd,
            address.as_mut_ptr().cast::<libc::sockaddr>(),
            &mut length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("inherited listener address cannot be inspected");
    }
    // SAFETY: getsockname initialized at least the family field.
    if unsafe { address.assume_init() }.ss_family as libc::c_int != libc::AF_UNIX {
        bail!("inherited listener is not a Unix-domain socket");
    }
    Ok(())
}

#[cfg(unix)]
fn socket_option(fd: libc::c_int, option: libc::c_int) -> Result<libc::c_int> {
    let mut value = 0;
    let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
    // SAFETY: value and length provide valid writable getsockopt storage.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("inherited listener socket option cannot be inspected");
    }
    Ok(value)
}

fn clear_activation_environment() {
    // SAFETY: this runs during single-threaded daemon listener initialization,
    // before any application worker can inspect or mutate the environment.
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_environment_is_exact() {
        let pid = std::process::id().to_string();
        assert!(validate_activation_environment(&pid, "1", "agentlibre").is_ok());
        assert!(validate_activation_environment("1", "1", "agentlibre").is_err());
        assert!(validate_activation_environment(&pid, "0", "agentlibre").is_err());
        assert!(validate_activation_environment(&pid, "2", "agentlibre").is_err());
        assert!(validate_activation_environment(&pid, "1", "wrong").is_err());
        assert!(validate_activation_environment(&pid, "1", "agentlibre;extra").is_err());
    }
}
