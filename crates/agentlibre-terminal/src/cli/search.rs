//! `agl search login` (AGL-043 F20): a client certificate of the agent
//! search from the step-ca of `search.ca`, through a login in the browser.
//! The `step` program does the login, the key and the request; agl keeps
//! the files where the user's `search` names them and writes their paths
//! there.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::{Map, Value};

use crate::cli::args::SearchAction;
use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::paths::Paths;
use crate::settings::{self, SearchCa, UserFile};
use crate::{outln, tr};

/// Where the key and the certificate go when the user's `search` names no
/// file of its own (AGL-043 F20), under the user's settings directory.
const CREDENTIALS: &str = "credentials/search";

/// The subject `step` is asked for. The OIDC provisioner of step-ca takes
/// the subject and the names of the certificate from the token (its `sub`
/// and, when there is one, its email), so this one is a placeholder.
const SUBJECT: &str = "agentLIBRE";

pub fn run(action: SearchAction, user: &UserFile, paths: &Paths, lang: Lang) -> Result<ExitCode, Error> {
    match action {
        SearchAction::Login => {
            let step = crate::programs::program("step")?;
            let done = login(&step, user, &paths.settings_file())?;
            outln!(
                "{}",
                tr!(
                    lang,
                    m::SEARCH_LOGIN_DONE,
                    cert = done.certificate.display(),
                    end = done.end
                )
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// What a login left: the certificate and the end of its validity.
#[derive(Debug, PartialEq, Eq)]
pub struct LoggedIn {
    pub certificate: PathBuf,
    pub end: String,
}

/// The login with the `step` program at `step`, for the user file at
/// `user_path`: the certificate and its key are made beside their final
/// place, the key turned into the PKCS#8 `[search]` reads (AGL-019, D4),
/// both made readable by the user alone, moved in, and their paths written
/// into the user's `search`.
pub fn login(step: &Path, user: &UserFile, user_path: &Path) -> Result<LoggedIn, Error> {
    let ca = user.search.ca.complete().map_err(|key| Error::SearchCaNotSet {
        user_path: user_path.into(),
        key: key.into(),
    })?;
    let dir = user_path
        .parent()
        .map(|config| config.join(CREDENTIALS))
        .unwrap_or_else(|| PathBuf::from(CREDENTIALS));
    let certificate = user
        .search
        .client_certificate
        .clone()
        .map_or_else(|| dir.join("client.crt"), PathBuf::from);
    let key = user
        .search
        .client_private_key
        .clone()
        .map_or_else(|| dir.join("client.key"), PathBuf::from);
    for parent in [certificate.parent(), key.parent()].into_iter().flatten() {
        fs::create_dir_all(parent).map_err(|source| Error::Write {
            path: parent.into(),
            source,
        })?;
    }
    let new_certificate = with_suffix(&certificate, ".new");
    let new_key = with_suffix(&key, ".new");
    let sec1 = with_suffix(&key, ".sec1");
    let result = issue(step, &ca, &new_certificate, &sec1)
        .and_then(|()| to_pkcs8(step, &sec1, &new_key))
        .and_then(|()| {
            for file in [&new_certificate, &new_key] {
                private(file)?;
            }
            rename(&new_certificate, &certificate)?;
            rename(&new_key, &key)
        });
    let _ = fs::remove_file(&sec1);
    if result.is_err() {
        let _ = fs::remove_file(&new_certificate);
        let _ = fs::remove_file(&new_key);
    }
    result?;
    write_search_paths(user_path, &certificate, &key)?;
    let end = validity_end(step, &certificate)?;
    Ok(LoggedIn { certificate, end })
}

/// `step ca certificate` against the step-ca: the browser login of its
/// OIDC provisioner, a new P-256 key and the certificate (AGL-043 F20).
/// The human's terminal is its terminal: it prints the address to open.
fn issue(step: &Path, ca: &SearchCa, certificate: &Path, key: &Path) -> Result<(), Error> {
    let mut command = Command::new(step);
    command
        .args(["ca", "certificate", SUBJECT])
        .arg(certificate)
        .arg(key)
        .args(["--provisioner", &ca.provisioner, "--ca-url", &ca.url])
        .arg("--root")
        .arg(&ca.root)
        .args(["--kty", "EC", "--curve", "P-256"])
        .args(["--no-password", "--insecure", "--force"]);
    crate::programs::clean_env(&mut command);
    let status = command.status().map_err(|source| Error::SearchLogin {
        detail: format!("cannot run {}: {source}", step.display()),
    })?;
    if !status.success() {
        return Err(Error::SearchLogin {
            detail: format!("step ca certificate ended with {status}"),
        });
    }
    Ok(())
}

/// The key `step` wrote (SEC1, `EC PRIVATE KEY`) as the PKCS#8
/// `PRIVATE KEY` of `[search]` (AGL-019, D4).
fn to_pkcs8(step: &Path, sec1: &Path, pkcs8: &Path) -> Result<(), Error> {
    let mut command = Command::new(step);
    command
        .args(["crypto", "key", "format"])
        .arg(sec1)
        .args(["--pkcs8", "--pem", "--no-password", "--insecure", "--force"])
        .arg("--out")
        .arg(pkcs8)
        .stdin(Stdio::null());
    crate::programs::clean_env(&mut command);
    captured(command, "step crypto key format").map(|_| ())
}

/// The end of the certificate's validity, as `step certificate inspect`
/// tells it (`validity.end`).
fn validity_end(step: &Path, certificate: &Path) -> Result<String, Error> {
    let mut command = Command::new(step);
    command
        .args(["certificate", "inspect"])
        .arg(certificate)
        .args(["--format", "json"])
        .stdin(Stdio::null());
    crate::programs::clean_env(&mut command);
    let output = captured(command, "step certificate inspect")?;
    let json: Value = serde_json::from_str(&output).map_err(|error| Error::SearchLogin {
        detail: format!("step certificate inspect: {error}"),
    })?;
    json["validity"]["end"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::SearchLogin {
            detail: "step certificate inspect told no validity.end".into(),
        })
}

/// Runs `command` and returns its stdout; a failure names `what` and
/// carries its stderr.
fn captured(mut command: Command, what: &str) -> Result<String, Error> {
    let output = command.output().map_err(|source| Error::SearchLogin {
        detail: format!("cannot run {what}: {source}"),
    })?;
    if !output.status.success() {
        return Err(Error::SearchLogin {
            detail: format!(
                "{what} ended with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The user's `search` with the paths of the certificate and the key; the
/// other keys stay (AGL-039, D12: the file is written anew).
fn write_search_paths(user_path: &Path, certificate: &Path, key: &Path) -> Result<(), Error> {
    let mut search = if user_path.exists() {
        match &crate::nix::eval_file(user_path)?["search"] {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        }
    } else {
        Map::new()
    };
    let path = |file: &Path| Value::String(file.to_string_lossy().into_owned());
    search.insert("client_certificate".into(), path(certificate));
    search.insert("client_private_key".into(), path(key));
    settings::set_top(user_path, "search", Value::Object(search))
}

/// The credential policy of `[search]` wants the files readable by the
/// user alone (AGL-019, D4).
fn private(file: &Path) -> Result<(), Error> {
    fs::set_permissions(file, fs::Permissions::from_mode(0o600)).map_err(|source| Error::Write {
        path: file.into(),
        source,
    })
}

fn rename(from: &Path, to: &Path) -> Result<(), Error> {
    fs::rename(from, to).map_err(|source| Error::Write {
        path: to.into(),
        source,
    })
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Whether the certificate at `certificate` has expired, as `step
/// certificate needs-renewal --expires-in 0s` tells it: exit 0 is expired,
/// 1 valid, 2 a missing file, 255 another error. Only an expired one is
/// `true`: the search reports its own errors (AGL-043 F20).
pub fn certificate_expired(step: &Path, certificate: &Path) -> bool {
    let mut command = Command::new(step);
    command
        .args(["certificate", "needs-renewal"])
        .arg(certificate)
        .args(["--expires-in", "0s"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::programs::clean_env(&mut command);
    command.status().is_ok_and(|status| status.code() == Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;
    use std::os::unix::fs::PermissionsExt;

    /// A `step` that writes what the real one would: a certificate and a
    /// SEC1 key for `ca certificate` (and its arguments to `args`), the
    /// PKCS#8 key for `crypto key format`, a validity for `certificate
    /// inspect`, and `needs-renewal` by the certificate's text.
    fn fake_step(dir: &Path) -> PathBuf {
        let step = dir.join("step");
        fs::write(
            &step,
            format!(
                r#"#!/bin/sh
case "$1 $2" in
  "ca certificate")
    echo "$@" > {args}
    echo "-----BEGIN CERTIFICATE-----" > "$4"
    echo "-----BEGIN EC PRIVATE KEY-----" > "$5" ;;
  "crypto key")
    out=""; prev=""
    for a in "$@"; do [ "$prev" = "--out" ] && out="$a"; prev="$a"; done
    echo "-----BEGIN PRIVATE KEY-----" > "$out" ;;
  "certificate inspect")
    echo '{{"validity": {{"end": "2026-11-10T21:00:00Z"}}}}' ;;
  "certificate needs-renewal")
    grep -q EXPIRED "$3" && exit 0; exit 1 ;;
esac
"#,
                args = dir.join("args").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&step, fs::Permissions::from_mode(0o755)).unwrap();
        step
    }

    fn user_with_ca(dir: &Path) -> (UserFile, PathBuf) {
        let path = dir.join("agentLIBRE").join("settings.nix");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{ agl, ... }: { search = { url = "https://s.example/v1/search"; private_ca = "/c/ca.crt"; ca = { url = "https://ca.example"; root = "/c/root.crt"; provisioner = "kanidm"; }; }; }"#,
        )
        .unwrap();
        (UserFile::load(&path).unwrap(), path)
    }

    #[test]
    fn a_login_keeps_the_files_and_writes_their_paths() {
        let dir = TempDir::new();
        let step = fake_step(&dir.0);
        let (user, path) = user_with_ca(&dir.0);
        let done = login(&step, &user, &path).unwrap();
        let credentials = dir.0.join("agentLIBRE").join(CREDENTIALS);
        assert_eq!(
            done,
            LoggedIn {
                certificate: credentials.join("client.crt"),
                end: "2026-11-10T21:00:00Z".into(),
            }
        );
        // `step` was asked with the step-ca of `search.ca`.
        let args = fs::read_to_string(dir.0.join("args")).unwrap();
        assert!(args.contains("--provisioner kanidm --ca-url https://ca.example --root /c/root.crt"), "{args}");
        assert!(args.contains("--kty EC --curve P-256"));
        // The key is the PKCS#8 one, both files the user's alone, no
        // leftovers.
        let key = credentials.join("client.key");
        assert_eq!(fs::read_to_string(&key).unwrap(), "-----BEGIN PRIVATE KEY-----\n");
        for file in [&key, &done.certificate] {
            assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let mut left: Vec<String> = fs::read_dir(&credentials)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["client.crt", "client.key"]);
        // The user's `search` names them, its other keys kept.
        let reloaded = UserFile::load(&path).unwrap();
        assert_eq!(
            reloaded.search.client_certificate.as_deref(),
            Some(done.certificate.to_str().unwrap())
        );
        assert_eq!(reloaded.search.client_private_key.as_deref(), Some(key.to_str().unwrap()));
        assert_eq!(reloaded.search.private_ca.as_deref(), Some("/c/ca.crt"));
        assert_eq!(reloaded.search.ca, user.search.ca);
        assert!(reloaded.search.complete().unwrap().is_some());
    }

    #[test]
    fn a_login_needs_the_step_ca() {
        let dir = TempDir::new();
        let step = fake_step(&dir.0);
        let path = dir.0.join("settings.nix");
        assert!(matches!(
            login(&step, &UserFile::default(), &path),
            Err(Error::SearchCaNotSet { key, .. }) if key == "url"
        ));
    }

    #[test]
    fn a_failed_step_leaves_no_files() {
        let dir = TempDir::new();
        let step = dir.0.join("step");
        fs::write(&step, "#!/bin/sh\nexit 3\n").unwrap();
        fs::set_permissions(&step, fs::Permissions::from_mode(0o755)).unwrap();
        let (user, path) = user_with_ca(&dir.0);
        assert!(matches!(login(&step, &user, &path), Err(Error::SearchLogin { .. })));
        let credentials = dir.0.join("agentLIBRE").join(CREDENTIALS);
        assert_eq!(fs::read_dir(&credentials).unwrap().count(), 0);
    }

    #[test]
    fn an_expired_certificate_is_told() {
        let dir = TempDir::new();
        let step = fake_step(&dir.0);
        let fresh = dir.0.join("fresh.crt");
        let old = dir.0.join("old.crt");
        fs::write(&fresh, "valid").unwrap();
        fs::write(&old, "EXPIRED").unwrap();
        assert!(!certificate_expired(&step, &fresh));
        assert!(certificate_expired(&step, &old));
        // A check that cannot run is no expiry.
        assert!(!certificate_expired(&dir.0.join("no-step"), &old));
    }
}
