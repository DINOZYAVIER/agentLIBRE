//! The download of a weights file a Function names (AGL-050, D8, D17): by
//! `curl`, from a url pinned to a version, into a `.part` file that resumes;
//! the file takes its name only after its sha256 is checked.

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::programs;
use crate::tr;

/// Where a weights file comes from: `source` of a model, under the local name
/// of the file (AGL-039, D29).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSource {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// `bytes` for a human, in decimal gigabytes as the sizes of the sources are.
pub fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// The file of `source` in `dir`: fetched, resumed and checked. A file already
/// there under its name is replaced only once the new one is checked.
pub fn fetch(source: &FileSource, dir: &Path, lang: Lang) -> Result<PathBuf, Error> {
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
    let available = free_space(dir).map_err(|error| failed(source, &error.to_string()))?;
    if available < needed {
        return Err(Error::DownloadNoSpace {
            name: source.name.clone(),
            needed,
            available,
            dir: dir.to_path_buf(),
        });
    }
    // A whole `.part` has nothing to resume: a server refuses the range.
    if needed > 0 {
        eprintln!(
            "{}",
            tr!(lang, m::DOWNLOAD_START, name = source.name, size = gigabytes(source.size))
        );
        run_curl(source, &part)?;
    }
    let got = checksum(source, &part)?;
    if got != source.sha256 {
        remove(source, &part)?;
        return Err(Error::DownloadMismatch {
            name: source.name.clone(),
            expected: source.sha256.clone(),
            got,
        });
    }
    fs::rename(&part, &path).map_err(|error| failed(source, &error.to_string()))?;
    Ok(path)
}

fn run_curl(source: &FileSource, part: &Path) -> Result<(), Error> {
    let mut command = Command::new(programs::program("curl")?);
    programs::clean_env(&mut command);
    let status = command
        .args(["--fail", "--location", "--progress-bar", "--retry", "5", "--retry-delay", "2"])
        .args(["--continue-at", "-", "--output"])
        .arg(part)
        .arg(&source.url)
        .status()
        .map_err(|error| failed(source, &error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(failed(source, &format!("curl: {status}")))
    }
}

fn checksum(source: &FileSource, part: &Path) -> Result<String, Error> {
    let mut command = Command::new(programs::program("sha256sum")?);
    programs::clean_env(&mut command);
    let output = command
        .arg(part)
        .output()
        .map_err(|error| failed(source, &error.to_string()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(failed(source, &detail));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.split_whitespace().next().unwrap_or_default().to_owned())
}

fn remove(source: &FileSource, path: &Path) -> Result<(), Error> {
    fs::remove_file(path).map_err(|error| failed(source, &error.to_string()))
}

fn failed(source: &FileSource, detail: &str) -> Error {
    Error::DownloadFailed {
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
        let path = fetch(&source, &models, Lang::En).unwrap();
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
        let path = fetch(&source, &models, Lang::En).unwrap();
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
        assert!(fetch(&source, &models, Lang::En).is_ok());
    }

    #[test]
    fn a_wrong_checksum_leaves_nothing_under_the_name_and_keeps_the_old_file() {
        let dir = TempDir::new();
        let mut source = served(&dir.0.join("server"), b"the weights");
        source.sha256 = "0".repeat(64);
        let models = dir.0.join("models");
        fs::create_dir_all(&models).unwrap();
        fs::write(models.join("m.gguf"), b"the old one").unwrap();
        let error = fetch(&source, &models, Lang::En).unwrap_err();
        assert!(matches!(error, Error::DownloadMismatch { .. }), "{error:?}");
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
        assert_eq!(fs::read(fetch(&source, &models, Lang::En).unwrap()).unwrap(), b"the weights");
    }

    #[test]
    fn a_file_too_big_for_the_disk_is_refused_before_it_starts() {
        let dir = TempDir::new();
        let mut source = served(&dir.0.join("server"), b"the weights");
        source.size = u64::MAX;
        let models = dir.0.join("models");
        let error = fetch(&source, &models, Lang::En).unwrap_err();
        assert!(matches!(error, Error::DownloadNoSpace { .. }), "{error:?}");
        assert!(!models.join("m.gguf.part").exists());
    }

    #[test]
    fn a_failed_download_is_an_error_with_its_name() {
        let dir = TempDir::new();
        let mut source = served(&dir.0.join("server"), b"the weights");
        source.url = "file:///nowhere".into();
        let error = fetch(&source, &dir.0.join("models"), Lang::En).unwrap_err();
        assert!(matches!(error, Error::DownloadFailed { name, .. } if name == "m.gguf"));
    }

    #[test]
    fn sizes_read_in_gigabytes() {
        assert_eq!(gigabytes(4_215_695_776), "4.2 GB");
    }
}
