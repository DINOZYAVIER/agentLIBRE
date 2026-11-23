//! The Windows engine of agl in WSL (AGL-052, D9, D10): `llama-server.exe` from
//! the official release, in the store, started from Linux by the interop of
//! WSL. It sees Windows paths, and it listens on the Windows side, which WSL
//! reaches at `127.0.0.1` only in the mirrored network.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentlibre_common::i18n::{Lang, Msg};
use agentlibre_common::tr;

/// What can go wrong starting or driving the Windows engine of agl in WSL
/// (AGL-052, D9, D10; AGL-056, D8; AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// The network of WSL is not the mirrored one (AGL-052, D10).
    NotMirrored { mode: String },
    /// Windows did not say where the weights go (AGL-056, D8).
    NoLocalAppData,
    /// `wslpath` did not convert a path (AGL-052, D10).
    Path { path: PathBuf },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NotMirrored { mode } => tr!(lang, NOT_MIRRORED, mode = mode),
            Self::NoLocalAppData => tr!(lang, NO_LOCAL_APP_DATA).to_string(),
            Self::Path { path } => tr!(lang, PATH, path = path.display()),
        }
    }
}

/// `Error::NotMirrored`.
const NOT_MIRRORED: Msg = Msg {
    en: "agl in WSL reaches the Windows llama-server only in the mirrored network, and this one is {mode}: put `networkingMode=mirrored` under `[wsl2]` in %UserProfile%\\.wslconfig, then run `wsl --shutdown`",
    uk: "agl у WSL бачить llama-server на Windows лише в дзеркальній мережі, а ця мережа {mode}: додай `networkingMode=mirrored` у `[wsl2]` файлу %UserProfile%\\.wslconfig і виконай `wsl --shutdown`",
};

/// `Error::NoLocalAppData`.
const NO_LOCAL_APP_DATA: Msg = Msg {
    en: "Windows does not say its %LOCALAPPDATA%, where the weights go; cmd.exe must run from WSL",
    uk: "Windows не повідомляє свій %LOCALAPPDATA%, куди йдуть ваги; cmd.exe має запускатися з WSL",
};

/// `Error::Path`.
const PATH: Msg = Msg {
    en: "wslpath did not convert {path}",
    uk: "wslpath не перетворив {path}",
};


/// Whether `program` is a Windows one.
fn is_windows_program(program: &Path) -> bool {
    program.extension().is_some_and(|extension| extension == "exe")
}

/// Whether the engine of the package runs on Windows (AGL-052, D9).
pub fn engine_is_windows() -> bool {
    std::env::var_os(agentlibre_common::programs::ENGINES_VAR)
        .is_some_and(|dir| Path::new(&dir).join("llama-server.exe").is_file())
}

/// The engine that runs on the Windows side of WSL (AGL-052, D9, D10;
/// AGL-121, D5): `new` checks before the launch, `command` makes the
/// command see Windows paths, and `stop` ends it by the port — Windows
/// knows no process of the Linux relay, so the port is the handle
/// (AGL-058, D13).
pub struct WindowsEngine {
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl WindowsEngine {
    /// The engine of `program` listening at `host`:`port`, when its name
    /// says Windows; `None` when it does not. The check of the network
    /// (D10) comes first: no engine starts on a WSL that cannot reach it.
    pub fn new(program: &Path, host: String, port: u16) -> Result<Option<WindowsEngine>, Error> {
        if !is_windows_program(program) {
            return Ok(None);
        }
        check_network()?;
        Ok(Some(WindowsEngine { host, port }))
    }

    /// `command` with its absolute paths, but for `program`, as Windows
    /// sees them (D9).
    pub fn command(
        &self,
        command: Vec<OsString>,
        program: &Path,
    ) -> Result<Vec<OsString>, Error> {
        windows_paths(command, program)
    }

    /// Ends what listens on the port when that is `llama-server`, and no
    /// other (AGL-058, D13).
    pub fn stop(&self) {
        stop_engine_on_port(self.port);
    }
}

/// agl's own weights folder (AGL-056, D7, D8): with an engine on Windows,
/// `%LOCALAPPDATA%\agentLIBRE\models`, which it reads directly and not through
/// `\\wsl.localhost`; else `linux`, under the data directory.
pub fn own_models(linux: PathBuf) -> Result<PathBuf, Error> {
    if !engine_is_windows() {
        return Ok(linux);
    }
    let local = Command::new("cmd.exe")
        // `/u`: UTF-16, as a pipe otherwise gets the OEM code page, which a
        // user name that is not ASCII would not survive (AGL-058, D25).
        .args(["/u", "/d", "/c", "echo %LOCALAPPDATA%"])
        // cmd.exe complains of the path of Linux it would start in.
        .current_dir("/mnt/c")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| from_utf16le(&output.stdout).trim().to_owned());
    models_under(local.as_deref(), &|windows| {
        let output = Command::new("wslpath").arg("-u").arg(windows).output().ok()?;
        output
            .status
            .success()
            .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
    })
}

/// The text of the bytes `cmd.exe /u` writes.
fn from_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

fn models_under(
    local: Option<&str>,
    to_linux: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<PathBuf, Error> {
    local
        // An unset variable comes back from cmd.exe as its own name.
        .filter(|local| !local.is_empty() && !local.contains('%'))
        .and_then(to_linux)
        .map(|local| local.join("agentLIBRE").join("models"))
        .ok_or(Error::NoLocalAppData)
}

/// The network of WSL must be the mirrored one (`wslinfo` tells).
fn check_network() -> Result<(), Error> {
    let mode = Command::new("wslinfo")
        .arg("--networking-mode")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|mode| !mode.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    if mode == "mirrored" {
        Ok(())
    } else {
        Err(Error::NotMirrored { mode })
    }
}

/// `command` with its absolute paths, but for `program`, as Windows sees them.
fn windows_paths(command: Vec<OsString>, program: &Path) -> Result<Vec<OsString>, Error> {
    convert_paths(command, program, &|path| {
        let output = Command::new("wslpath").arg("-w").arg(path).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().into())
    })
}

fn convert_paths(
    command: Vec<OsString>,
    program: &Path,
    convert: &dyn Fn(&OsString) -> Option<OsString>,
) -> Result<Vec<OsString>, Error> {
    command
        .into_iter()
        .map(|arg| {
            if arg == program || !Path::new(&arg).is_absolute() {
                return Ok(arg);
            }
            convert(&arg).ok_or_else(|| Error::Path { path: arg.into() })
        })
        .collect()
}

/// The PowerShell that ends what listens on `port` when that is `llama-server`,
/// and says so.
fn stop_script(port: u16) -> String {
    format!(
        "$owner = (Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess; \
         if ($owner -and (Get-Process -Id $owner).ProcessName -eq 'llama-server') {{ \
         Stop-Process -Id $owner -Force; Write-Output stopped }}"
    )
}

/// Ends the engine on the Windows side that listens on `port`, and no other
/// (AGL-058, D13): Windows knows no process of Linux, so it goes by the port.
/// Whether one was ended.
fn stop_engine_on_port(port: u16) -> bool {
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &stop_script(port)])
        .current_dir("/mnt/c")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("stopped"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_paths_past_the_program_become_windows_ones() {
        let command: Vec<OsString> = ["/nix/store/x/llama-server.exe", "-m", "/m/a.gguf", "-c", "4096", "--mmproj", "/m/p.gguf"]
            .map(OsString::from)
            .to_vec();
        let program = Path::new("/nix/store/x/llama-server.exe");
        let converted = convert_paths(command.clone(), program, &|path| {
            Some(format!("W:{}", path.to_string_lossy()).into())
        })
        .unwrap();
        let expected: Vec<OsString> = ["/nix/store/x/llama-server.exe", "-m", "W:/m/a.gguf", "-c", "4096", "--mmproj", "W:/m/p.gguf"]
            .map(OsString::from)
            .to_vec();
        assert_eq!(converted, expected);
        // A path that cannot be converted is an error, not a wrong argument.
        assert!(matches!(
            convert_paths(command, program, &|_| None),
            Err(Error::Path { path }) if path == Path::new("/m/a.gguf")
        ));
    }

    #[test]
    fn the_weights_are_under_the_local_app_data_of_windows() {
        let to_linux = |windows: &str| {
            assert_eq!(windows, r"C:\Users\me\AppData\Local");
            Some(PathBuf::from("/mnt/c/Users/me/AppData/Local"))
        };
        assert_eq!(
            models_under(Some(r"C:\Users\me\AppData\Local"), &to_linux).unwrap(),
            Path::new("/mnt/c/Users/me/AppData/Local/agentLIBRE/models")
        );
        // Nothing to go by is an error, not the folder of Linux.
        let never = |_: &str| -> Option<PathBuf> { panic!("nothing to convert") };
        for local in [None, Some(""), Some("%LOCALAPPDATA%")] {
            assert!(matches!(models_under(local, &never), Err(Error::NoLocalAppData)));
        }
        let cannot = |_: &str| None;
        assert!(matches!(models_under(Some(r"C:\x"), &cannot), Err(Error::NoLocalAppData)));
    }

    #[test]
    fn local_app_data_keeps_a_name_that_is_not_ascii() {
        // What `cmd.exe /u` writes (AGL-058, D25).
        let path = "C:\\Users\\Олена\\AppData\\Local\r\n";
        let bytes: Vec<u8> = path.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(from_utf16le(&bytes).trim(), "C:\\Users\\Олена\\AppData\\Local");
    }

    #[test]
    fn the_stop_goes_by_the_port_and_checks_the_program() {
        // AGL-058, D13.
        let script = stop_script(18080);
        assert!(script.contains("-LocalPort 18080"), "{script}");
        assert!(script.contains("-eq 'llama-server'"), "{script}");
        assert!(!script.contains("taskkill"), "{script}");
    }

    #[test]
    fn a_windows_program_is_told_by_its_name() {
        assert!(is_windows_program(Path::new("/nix/store/x/llama-server.exe")));
        assert!(!is_windows_program(Path::new("/nix/store/x/llama-server")));
    }
}
