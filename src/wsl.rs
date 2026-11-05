//! The Windows engine of agl in WSL (AGL-052, D9, D10): `llama-server.exe` from
//! the official release, in the store, started from Linux by the interop of
//! WSL. It sees Windows paths, and it listens on the Windows side, which WSL
//! reaches at `127.0.0.1` only in the mirrored network.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::error::Error;

/// Whether `program` is a Windows one.
pub fn is_windows_program(program: &Path) -> bool {
    program.extension().is_some_and(|extension| extension == "exe")
}

/// The network of WSL must be the mirrored one (`wslinfo` tells).
pub fn check_network() -> Result<(), Error> {
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
        Err(Error::WslNotMirrored { mode })
    }
}

/// `command` with its absolute paths, but for `program`, as Windows sees them.
pub fn windows_paths(command: Vec<OsString>, program: &Path) -> Result<Vec<OsString>, Error> {
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
            convert(&arg).ok_or_else(|| Error::WslPath { path: arg.into() })
        })
        .collect()
}

/// Ends the engine on the Windows side, when ending its relay did not. It goes
/// by the name of the program: Windows knows no process of Linux.
pub fn stop_windows_engine() {
    let _ = Command::new("taskkill.exe")
        .args(["/F", "/IM", "llama-server.exe"])
        .output();
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
            Err(Error::WslPath { path }) if path == Path::new("/m/a.gguf")
        ));
    }

    #[test]
    fn a_windows_program_is_told_by_its_name() {
        assert!(is_windows_program(Path::new("/nix/store/x/llama-server.exe")));
        assert!(!is_windows_program(Path::new("/nix/store/x/llama-server")));
    }
}
