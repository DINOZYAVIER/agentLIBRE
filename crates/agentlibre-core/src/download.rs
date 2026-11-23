//! The download of a weights file a Function names (AGL-050, D8, D17): by
//! `curl`, from a url pinned to a version, into a `.part` file that resumes;
//! the file takes its name only after its sha256 is checked.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error;
use agentlibre_common::i18n::{Lang, Msg, m};
use crate::programs;
use agentlibre_common::tr;

/// What can go wrong fetching a weights file (AGL-050, D17; AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// `curl` or the file system failed on a download (AGL-050, D17).
    Failed { name: String, detail: String },
    /// The disk has too little room for a download (AGL-050, D17).
    NoSpace {
        name: String,
        needed: u64,
        available: u64,
        dir: PathBuf,
    },
    /// A downloaded file has another sha256 (AGL-050, D17).
    Mismatch {
        name: String,
        expected: String,
        got: String,
    },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Failed { name, detail } => tr!(lang, FAILED, name = name, detail = detail),
            Self::NoSpace { name, needed, available, dir } => tr!(
                lang,
                NO_SPACE,
                name = name,
                needed = gigabytes(*needed),
                dir = dir.display(),
                available = gigabytes(*available)
            ),
            Self::Mismatch { name, expected, got } => {
                tr!(lang, MISMATCH, name = name, got = got, expected = expected)
            }
        }
    }
}

/// `Error::Failed`.
const FAILED: Msg = Msg {
    en: "the download of {name} failed: {detail}",
    uk: "завантаження {name} не вдалось: {detail}",
};

/// `Error::NoSpace`.
const NO_SPACE: Msg = Msg {
    en: "{name} needs {needed} in {dir}, and {available} is free",
    uk: "для {name} потрібно {needed} у {dir}, а вільно {available}",
};

/// `Error::Mismatch`.
const MISMATCH: Msg = Msg {
    en: "the sha256 of the downloaded {name} is {got}, not {expected}; it was not kept",
    uk: "sha256 завантаженого {name} — {got}, а не {expected}; файл не збережено",
};

/// Where a weights file comes from: `source` of a model, under the local name
/// of the file (AGL-039, D29).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSource {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// What `curl` exits with when a server does not take a range.
const CURL_NO_RANGES: i32 = 33;
/// What `curl --fail` exits with on an error status.
const CURL_HTTP_ERROR: i32 = 22;

/// `bytes` for a human, in decimal gigabytes as the sizes of the sources are.
pub fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// The file of `source` in `dir`: fetched, resumed and checked. A file already
/// there under its name is replaced only once the new one is checked.
pub fn fetch(
    source: &FileSource,
    dir: &Path,
    lang: Lang,
    say: &dyn Fn(&str),
) -> Result<PathBuf, error::Error> {
    fs::create_dir_all(dir).map_err(|error| failed(source, &error.to_string()))?;
    let path = dir.join(&source.name);
    let part = dir.join(format!("{}.part", source.name));
    let have = fs::metadata(&part).map_or(0, |metadata| metadata.len());
    // Longer than the file cannot be the start of it.
    let have = if have > source.size {
        remove(source, &part)?;
        0
    } else {
        have
    };
    let needed = source.size - have;
    check_room(&source.name, needed, dir).map_err(|error| match error {
        Room::Io(detail) => failed(source, &detail),
        Room::No(error) => error,
    })?;
    // A whole `.part` has nothing to resume: a server refuses the range.
    if needed > 0 {
        say(&tr!(lang, m::DOWNLOAD_START, name = source.name, size = gigabytes(source.size)));
        run_curl(source, &part)?;
    }
    // It takes a while, with nothing else on the screen (AGL-058, D26).
    say(&tr!(lang, m::DOWNLOAD_CHECK, name = source.name));
    let got = checksum(source, &part)?;
    if got != source.sha256 {
        remove(source, &part)?;
        return Err(Error::Mismatch {
            name: source.name.clone(),
            expected: source.sha256.clone(),
            got,
        }
        .into());
    }
    fs::rename(&part, &path).map_err(|error| failed(source, &error.to_string()))?;
    Ok(path)
}

fn run_curl(source: &FileSource, part: &Path) -> Result<(), error::Error> {
    let mut command = Command::new(programs::program("curl")?);
    programs::clean_env(&mut command);
    let status = command
        .args(["--fail", "--location", "--progress-bar", "--retry", "5", "--retry-delay", "2"])
        .args(["--continue-at", "-", "--output"])
        .arg(part)
        .arg(&source.url)
        .status()
        .map_err(|error| failed(source, &error.to_string()))?;
    match status.code() {
        Some(0) => Ok(()),
        // A server with no ranges would refuse every next try (AGL-058, D26).
        Some(CURL_NO_RANGES) => {
            remove(source, part)?;
            Err(failed(source, "this server cannot resume, so the partial file is removed; run it again to start over").into())
        }
        Some(CURL_HTTP_ERROR) => Err(failed(source, "the server refuses the file").into()),
        _ => Err(failed(source, &format!("curl: {status}; run it again and it resumes")).into()),
    }
}

/// Why there is no room for a file.
enum Room {
    Io(String),
    No(Error),
}

fn check_room(name: &str, needed: u64, dir: &Path) -> Result<(), Room> {
    let available = free_space(dir).map_err(|error| Room::Io(error.to_string()))?;
    if available < needed {
        return Err(Room::No(Error::NoSpace {
            name: name.to_owned(),
            needed,
            available,
            dir: dir.to_path_buf(),
        }));
    }
    Ok(())
}

/// Whether all of `sources` fit in `dir`, what their parts hold counted: asked
/// before the first file comes down, so that the last is not refused after it
/// (AGL-050, D17; AGL-058, D26).
pub fn check_space(sources: &[&FileSource], dir: &Path) -> Result<(), error::Error> {
    let Some(first) = sources.first() else {
        return Ok(());
    };
    fs::create_dir_all(dir).map_err(|error| failed(first, &error.to_string()))?;
    let needed = sources.iter().fold(0u64, |sum, source| {
        let part = dir.join(format!("{}.part", source.name));
        let have = fs::metadata(part).map_or(0, |metadata| metadata.len());
        sum.saturating_add(source.size.saturating_sub(have))
    });
    let names: Vec<&str> = sources.iter().map(|source| source.name.as_str()).collect();
    check_room(&names.join(", "), needed, dir).map_err(|error| match error {
        Room::Io(detail) => failed(first, &detail),
        Room::No(error) => error,
    })?;
    Ok(())
}

fn checksum(source: &FileSource, part: &Path) -> Result<String, error::Error> {
    let mut command = Command::new(programs::program("sha256sum")?);
    programs::clean_env(&mut command);
    let output = command
        .arg(part)
        .output()
        .map_err(|error| failed(source, &error.to_string()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(failed(source, &detail).into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.split_whitespace().next().unwrap_or_default().to_owned())
}

fn remove(source: &FileSource, path: &Path) -> Result<(), Error> {
    fs::remove_file(path).map_err(|error| failed(source, &error.to_string()))
}

fn failed(source: &FileSource, detail: &str) -> Error {
    Error::Failed {
        name: source.name.clone(),
        detail: detail.to_owned(),
    }
}

/// The bytes free for this user on the file system of `dir`.
fn free_space(dir: &Path) -> std::io::Result<u64> {
    let path = CString::new(dir.as_os_str().as_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string and `stat` a writable struct.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;
    use crate::media::sha256_hex;

    /// A source served by `file://`, which `curl` resumes as it does https.
    fn served(dir: &Path, bytes: &[u8]) -> FileSource {
        fs::create_dir_all(dir).unwrap();
        let origin = dir.join("origin.bin");
        fs::write(&origin, bytes).unwrap();
        FileSource {
            name: "m.gguf".into(),
            url: format!("file://{}", origin.display()),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
        }
    }

    #[test]
    fn a_file_comes_under_its_name_with_no_part_left() {
        let dir = TempDir::new();
        let source = served(&dir.0.join("server"), b"the weights");
        let models = dir.0.join("models");
        let path = fetch(&source, &models, Lang::En, &|_| {}).unwrap();
        assert_eq!(path, models.join("m.gguf"));
        assert_eq!(fs::read(&path).unwrap(), b"the weights");
        assert!(!models.join("m.gguf.part").exists());
    }

    #[test]
    fn an_interrupted_download_resumes() {
        let dir = TempDir::new();
        let source = served(&dir.0.join("server"), b"the weights");
        let models = dir.0.join("models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("m.gguf.part"), b"the wei").unwrap();
        let path = fetch(&source, &models, Lang::En, &|_| {}).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"the weights");
    }

    #[test]
    fn a_whole_part_is_only_checked() {
        let dir = TempDir::new();
        let mut source = served(&dir.0.join("server"), b"the weights");
        source.url = "file:///nowhere".into();
        let models = dir.0.join("models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("m.gguf.part"), b"the weights").unwrap();
        assert!(fetch(&source, &models, Lang::En, &|_| {}).is_ok());
    }

    #[test]
    fn a_wrong_checksum_leaves_nothing_under_the_name_and_keeps_the_old_file() {
        let dir = TempDir::new();
        let mut source = served(&dir.0.join("server"), b"the weights");
        source.sha256 = "0".repeat(64);
        let models = dir.0.join("models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("m.gguf"), b"the old one").unwrap();
        let error = fetch(&source, &models, Lang::En, &|_| {}).unwrap_err();
        assert!(matches!(error, error::Error::Download(Error::Mismatch { .. })), "{error:?}");
        assert_eq!(fs::read(models.join("m.gguf")).unwrap(), b"the old one");
        assert!(!models.join("m.gguf.part").exists());
    }

    #[test]
    fn a_part_longer_than_the_file_starts_over() {
        let dir = TempDir::new();
        let source = served(&dir.0.join("server"), b"the weights");
        let models = dir.0.join("models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("m.gguf.part"), b"not the weights at all").unwrap();
        assert_eq!(fs::read(fetch(&source, &models, Lang::En, &|_| {}).unwrap()).unwrap(), b"the weights");
    }

    #[test]
    fn a_file_too_big_for_the_disk_is_refused_before_it_starts() {
        let dir = TempDir::new();
        let mut source = served(&dir.0.join("server"), b"the weights");
        source.size = u64::MAX;
        let models = dir.0.join("models");
        let error = fetch(&source, &models, Lang::En, &|_| {}).unwrap_err();
        assert!(matches!(error, error::Error::Download(Error::NoSpace { .. })), "{error:?}");
        assert!(!models.join("m.gguf.part").exists());
    }

    #[test]
    fn a_failed_download_is_an_error_with_its_name() {
        let dir = TempDir::new();
        let mut source = served(&dir.0.join("server"), b"the weights");
        source.url = "file:///nowhere".into();
        let error = fetch(&source, &dir.0.join("models"), Lang::En, &|_| {}).unwrap_err();
        assert!(matches!(error, error::Error::Download(Error::Failed { name, .. }) if name == "m.gguf"));
    }

    #[test]
    fn the_space_is_asked_for_all_the_files_at_once() {
        // Each fits by itself, and both do not (AGL-058, D26).
        let dir = std::env::temp_dir().join(format!("agl-space-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let free = free_space(&dir).unwrap();
        let source = |name: &str| FileSource {
            name: name.into(),
            url: "http://127.0.0.1:9/x".into(),
            sha256: "0".repeat(64),
            size: free / 3 * 2,
        };
        let (one, two) = (source("one"), source("two"));
        assert!(check_space(&[&one], &dir).is_ok());
        assert!(matches!(
            check_space(&[&one, &two], &dir),
            Err(error::Error::Download(Error::NoSpace { name, .. })) if name == "one, two"
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sizes_read_in_gigabytes() {
        assert_eq!(gigabytes(4_215_695_776), "4.2 GB");
    }
}
