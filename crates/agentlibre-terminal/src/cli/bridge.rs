//! `agl bridge start|stop|status|pair <name>|revoke <name>|list` (AGL-083,
//! D10, D15, D18 to D21): the bridge of the iPhone and the watch, a
//! transient systemd user unit running `agl-bridge`, and its devices.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentlibre_bridge::{Store, StoreError, pairing_json};
use agentlibre_surface::daemon;
use qrcode::{Color, EcLevel, QrCode};

use crate::engine::{port_open, unit_active};
use crate::error::Error;
use crate::error::CoreError;
use crate::engine::error::Error as EngineError;
use agentlibre_common::i18n::{Lang, m};
use crate::outln;
use agentlibre_common::paths::Paths;
use agentlibre_common::settings::{BridgeFile, UserFile};
use agentlibre_common::tr;

/// The bridge's unit: one a user.
pub const UNIT: &str = "agentLIBRE-bridge";
/// The program, beside `agl`.
const PROGRAM: &str = "agl-bridge";
/// How long a started bridge may take to listen.
const START_WAIT: Duration = Duration::from_secs(5);

/// The sandbox of the unit (D8): nothing written, the home read only, its
/// own temporary files, no new privileges, the addresses of a socket and of
/// the network only.
const HARDENING: [&str; 10] = [
    "NoNewPrivileges=yes",
    "ProtectSystem=strict",
    "ProtectHome=read-only",
    "PrivateTmp=yes",
    "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
    "RestrictNamespaces=yes",
    "LockPersonality=yes",
    "MemoryDenyWriteExecute=yes",
    "SystemCallArchitectures=native",
    "RestrictRealtime=yes",
];

/// What `agl bridge` does.
#[derive(Debug, PartialEq, Eq)]
pub enum BridgeAction {
    Start,
    Stop,
    Status,
    Pair { name: String },
    Revoke { name: String },
    List,
}

pub fn run(action: BridgeAction, user: &UserFile, paths: &Paths, lang: Lang) -> Result<ExitCode, Error> {
    let store = Store::new(paths.config.join("bridge"));
    let settings = || Listen::of(&user.bridge, &paths.settings_file());
    match action {
        BridgeAction::Start => {
            let listen = settings()?;
            if unit_active(UNIT)? {
                outln!("{}", tr!(lang, m::BRIDGE_ALREADY_RUNNING, url = listen.url()));
                return Ok(ExitCode::SUCCESS);
            }
            store.certificate().map_err(store_error)?;
            let runtime = paths.runtime.as_deref().ok_or(Error::from(CoreError::DaemonNoRuntime))?;
            // The daemon first: the bridge never starts one (D18).
            drop(daemon::connect(paths, false, lang, true)?);
            let program = program()?;
            let socket = agentlibre_core::daemon::socket_path(runtime);
            start(&program, &listen, store.dir(), &socket)?;
            outln!("{}", tr!(lang, m::BRIDGE_STARTED, url = listen.url()));
            Ok(ExitCode::SUCCESS)
        }
        BridgeAction::Stop => {
            if unit_active(UNIT)? {
                systemctl(&["stop", UNIT])?;
                outln!("{}", tr!(lang, m::BRIDGE_STOPPED));
            } else {
                outln!("{}", tr!(lang, m::BRIDGE_NOT_RUNNING));
            }
            Ok(ExitCode::SUCCESS)
        }
        BridgeAction::Status => {
            let devices = store.devices().map_err(store_error)?.len();
            if unit_active(UNIT)? {
                let url = settings().map(|listen| listen.url()).unwrap_or_default();
                outln!("{}", tr!(lang, m::BRIDGE_STATUS_RUNNING, url = url, devices = devices));
            } else {
                outln!("{}", tr!(lang, m::BRIDGE_STATUS_STOPPED, devices = devices));
            }
            Ok(ExitCode::SUCCESS)
        }
        BridgeAction::Pair { name } => {
            let listen = settings()?;
            let certificate = store.certificate().map_err(store_error)?;
            let token = store.pair(&name).map_err(store_error)?;
            let json = pairing_json(&listen.url(), &token, &certificate.sha256(), &name);
            outln!("{}", qr(&json));
            outln!("{}", tr!(lang, m::BRIDGE_PAIRED, name = name, url = listen.url()));
            Ok(ExitCode::SUCCESS)
        }
        BridgeAction::Revoke { name } => {
            store.revoke(&name).map_err(store_error)?;
            outln!("{}", tr!(lang, m::BRIDGE_REVOKED, name = name));
            Ok(ExitCode::SUCCESS)
        }
        BridgeAction::List => {
            let devices = store.devices().map_err(store_error)?;
            if devices.is_empty() {
                outln!("{}", tr!(lang, m::BRIDGE_NO_DEVICES));
            }
            for device in devices {
                outln!("{}", device.name);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Where the bridge listens: `bridge` of the user's settings, both keys
/// set (D20).
#[derive(Debug, PartialEq, Eq)]
struct Listen {
    address: String,
    port: u16,
}

impl Listen {
    fn of(bridge: &BridgeFile, path: &Path) -> Result<Listen, Error> {
        match (&bridge.address, bridge.port) {
            (Some(address), Some(port)) => Ok(Listen {
                address: address.clone(),
                port,
            }),
            _ => Err(Error::BridgeNotSet { path: path.into() }),
        }
    }

    /// The address and the port as a socket's, an IPv6 address in brackets.
    fn authority(&self) -> String {
        if self.address.contains(':') {
            format!("[{}]:{}", self.address, self.port)
        } else {
            format!("{}:{}", self.address, self.port)
        }
    }

    /// The url of the QR code (D21).
    fn url(&self) -> String {
        format!("wss://{}", self.authority())
    }
}

/// `agl-bridge`, beside the running `agl`: in the package and in a build
/// of the workspace alike.
fn program() -> Result<PathBuf, Error> {
    let exe = std::env::current_exe().map_err(|source| Error::Bridge {
        detail: source.to_string(),
    })?;
    let program = exe.with_file_name(PROGRAM);
    if program.is_file() {
        Ok(program)
    } else {
        Err(Error::BridgeProgramMissing { path: program })
    }
}

/// The unit's `systemd-run`: the hardening, then `agl-bridge` and its
/// arguments.
fn start_args(program: &Path, listen: &Listen, dir: &Path, socket: &Path) -> Vec<String> {
    let mut args = vec![
        "--user".to_owned(),
        format!("--unit={UNIT}"),
        "--collect".to_owned(),
        "--quiet".to_owned(),
    ];
    for property in HARDENING {
        args.push(format!("--property={property}"));
    }
    args.push("--".to_owned());
    args.push(program.display().to_string());
    args.push("--listen".to_owned());
    args.push(listen.authority());
    args.push("--dir".to_owned());
    args.push(dir.display().to_string());
    args.push("--socket".to_owned());
    args.push(socket.display().to_string());
    args
}

/// Starts the unit and waits until it listens.
fn start(program: &Path, listen: &Listen, dir: &Path, socket: &Path) -> Result<(), Error> {
    let status = Command::new("systemd-run")
        .args(start_args(program, listen, dir, socket))
        .stdin(Stdio::null())
        .status()
        .map_err(|source| crate::error::Error::from(EngineError::Systemd {
            detail: source.to_string(),
        }))?;
    if !status.success() {
        return Err(EngineError::Systemd {
            detail: format!("systemd-run: {status}"),
        }
        .into());
    }
    let started = Instant::now();
    while started.elapsed() < START_WAIT {
        if port_open(&listen.address, listen.port) {
            return Ok(());
        }
        if !unit_active(UNIT)? {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(Error::BridgeNotListening { unit: UNIT.into() })
}

fn systemctl(args: &[&str]) -> Result<(), Error> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|source| crate::error::Error::from(EngineError::Systemd {
            detail: source.to_string(),
        }))?;
    if status.success() {
        Ok(())
    } else {
        Err(EngineError::Systemd {
            detail: format!("systemctl {}: {status}", args.join(" ")),
        }
        .into())
    }
}

fn store_error(error: StoreError) -> Error {
    match error {
        StoreError::InvalidName(name) => Error::BridgeDeviceName { name },
        StoreError::Exists(name) => Error::BridgeDeviceExists { name },
        StoreError::Unknown(name) => Error::BridgeDeviceUnknown { name },
        other => Error::Bridge {
            detail: other.to_string(),
        },
    }
}

/// `text` as a QR code of the terminal, two rows of modules a line, black
/// on white whatever the terminal's colors, with its quiet zone.
fn qr(text: &str) -> String {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L)
        .expect("the pairing's JSON fits a QR code");
    let width = code.width();
    let colors = code.to_colors();
    const QUIET: usize = 4;
    let side = width + 2 * QUIET;
    let dark = |x: usize, y: usize| -> bool {
        x >= QUIET
            && y >= QUIET
            && x < QUIET + width
            && y < QUIET + width
            && colors[(y - QUIET) * width + (x - QUIET)] == Color::Dark
    };
    let mut out = String::new();
    for y in (0..side).step_by(2) {
        for x in 0..side {
            // The upper module is the glyph's, the lower the background.
            let fg = if dark(x, y) { 30 } else { 97 };
            let bg = if y + 1 < side && dark(x, y + 1) { 40 } else { 107 };
            out.push_str(&format!("\x1b[{fg};{bg}m\u{2580}"));
        }
        out.push_str("\x1b[0m\n");
    }
    out.pop();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listen(address: &str) -> Listen {
        Listen {
            address: address.into(),
            port: 18443,
        }
    }

    #[test]
    fn the_url_brackets_an_ipv6_address() {
        assert_eq!(listen("10.0.0.5").url(), "wss://10.0.0.5:18443");
        assert_eq!(listen("fd00::5").url(), "wss://[fd00::5]:18443");
        assert_eq!(listen("fd00::5").authority(), "[fd00::5]:18443");
    }

    #[test]
    fn both_keys_are_needed() {
        let path = Path::new("/c/settings.nix");
        let set = BridgeFile {
            address: Some("10.0.0.5".into()),
            port: Some(18443),
        };
        assert_eq!(Listen::of(&set, path).unwrap(), listen("10.0.0.5"));
        for half in [
            BridgeFile {
                address: Some("10.0.0.5".into()),
                port: None,
            },
            BridgeFile {
                address: None,
                port: Some(1),
            },
            BridgeFile::default(),
        ] {
            assert!(matches!(Listen::of(&half, path), Err(Error::BridgeNotSet { .. })));
        }
    }

    #[test]
    fn the_unit_is_hardened_and_runs_the_bridge_with_its_arguments() {
        let args = start_args(
            Path::new("/p/agl-bridge"),
            &listen("10.0.0.5"),
            Path::new("/c/bridge"),
            Path::new("/r/agl.sock"),
        );
        let split = args.iter().position(|arg| arg == "--").unwrap();
        assert!(args[..split].contains(&"--unit=agentLIBRE-bridge".to_owned()));
        assert!(args[..split].contains(&"--property=ProtectHome=read-only".to_owned()));
        assert!(args[..split].contains(&"--property=ProtectSystem=strict".to_owned()));
        assert_eq!(
            args[split + 1..],
            ["/p/agl-bridge", "--listen", "10.0.0.5:18443", "--dir", "/c/bridge", "--socket", "/r/agl.sock"]
        );
    }

    #[test]
    fn the_qr_code_is_square_with_its_quiet_zone() {
        let json = pairing_json("wss://10.0.0.5:18443", &"a".repeat(64), &"b".repeat(64), "iphone");
        let width = QrCode::with_error_correction_level(json.as_bytes(), EcLevel::L)
            .unwrap()
            .width();
        let text = qr(&json);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), (width + 8).div_ceil(2));
        for line in lines {
            assert_eq!(line.matches('\u{2580}').count(), width + 8);
        }
    }
}
