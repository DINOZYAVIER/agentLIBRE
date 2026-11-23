//! The registry of servers agl runs (AGL-121, D13): the record of the
//! launch in the project's store (AGL-035, D1) and the files below the
//! runtime directory that tell other programs which servers run
//! (AGL-064), read by `url_of_id` and `rebind_url` (AGL-120, D5).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;
use agentlibre_common::settings::ServerFile;

use super::model_of;
use agentlibre_common::error::Error as CommonError;

/// The record of what the running server was launched with, beside the
/// engine log in the project's store directory (AGL-035, D1).
const STATE_FILE: &str = "server.state";

/// Below the runtime directory, the directory of the files that tell
/// other programs which servers agl started run (AGL-064, D1).
const SERVERS_DIR: &str = "servers";

/// The record of what the running server was launched with (AGL-035,
/// D1): `pid` and the effective server, as JSON (AGL-041, D2).
pub(super) fn write_record(dir: &Path, pid: u32, effective: &ServerFile) -> Result<(), Error> {
    let path = dir.join(STATE_FILE);
    let record = serde_json::json!({ "pid": pid, "server": effective.to_json() });
    fs::write(&path, format!("{record}\n")).map_err(|source| Error::from(CommonError::Write { path, source }))
}

/// What the running server was launched with (AGL-035, D1): `None` when
/// there is no record, it cannot be read, or its process is gone.
pub fn read_record(dir: &Path) -> Option<ServerFile> {
    let path = dir.join(STATE_FILE);
    let record: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let pid = record.get("pid")?.as_u64()?;
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return None;
    }
    ServerFile::from_json(record.get("server")?)
}

/// The record goes away with the server it describes (AGL-035, D1), and
/// its file in the runtime directory (AGL-064, D3). `pid` is the server's
/// when the caller has it in hand (the child, the unit); else the record's.
pub fn remove_record(dir: &Path, runtime: Option<&Path>, pid: Option<u32>) {
    let pid = pid.or_else(|| record_pid(dir));
    let _ = fs::remove_file(dir.join(STATE_FILE));
    remove_runtime_file(runtime, pid);
}

/// The pid of the record, whether its process runs or not.
fn record_pid(dir: &Path) -> Option<u32> {
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join(STATE_FILE)).ok()?).ok()?;
    u32::try_from(record.get("pid")?.as_u64()?).ok()
}

/// The directory of the files of the running servers (AGL-064, D1).
fn servers_dir(runtime: &Path) -> PathBuf {
    runtime.join(SERVERS_DIR)
}

/// What the file of a running server holds (AGL-064, D2): `pid`, the
/// `kind` and the `url` of the effective server, and its `model` as
/// `model_of` names it, `null` where the settings name none.
fn runtime_file(pid: u32, effective: &ServerFile) -> Result<serde_json::Value, Error> {
    Ok(serde_json::json!({
        "pid": pid,
        "kind": effective.kind().name(),
        "url": effective.url(),
        "model": model_of(effective)?,
    }))
}

/// Tells other programs that the server of `pid` runs (AGL-064, D1, D2):
/// `servers/<pid>.json` below the runtime directory, written whole to a
/// temporary name and renamed, so that a reader never sees half of it.
/// Nothing where there is no runtime directory (D7). The files of the
/// processes that are gone go first (D4).
pub(super) fn write_runtime_file(runtime: Option<&Path>, pid: u32, effective: &ServerFile) -> Result<(), Error> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    let dir = servers_dir(runtime);
    fs::create_dir_all(&dir).map_err(|source| Error::from(CommonError::Write {
        path: dir.clone(),
        source,
    }))?;
    sweep_runtime_files(&dir);
    let temporary = dir.join(format!("{pid}.json.tmp"));
    let path = dir.join(format!("{pid}.json"));
    fs::write(&temporary, format!("{}\n", runtime_file(pid, effective)?)).map_err(|source| {
        CommonError::Write {
            path: temporary.clone(),
            source,
        }
    })?;
    fs::rename(&temporary, &path).map_err(|source| Error::from(CommonError::Write { path, source }))
}

/// Takes the file of the server of `pid`, when the caller knows it, and
/// the files of the processes that are gone away (AGL-064, D3, D4).
pub(super) fn remove_runtime_file(runtime: Option<&Path>, pid: Option<u32>) {
    let Some(runtime) = runtime else {
        return;
    };
    let dir = servers_dir(runtime);
    if let Some(pid) = pid {
        let _ = fs::remove_file(dir.join(format!("{pid}.json")));
    }
    sweep_runtime_files(&dir);
}

/// Removes the `<pid>.json` files of `dir` whose process is gone
/// (`/proc/<pid>`, as `read_record` judges; AGL-064, D4).
fn sweep_runtime_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let pid = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|pid| pid.parse::<u32>().ok());
        if let Some(pid) = pid
            && !Path::new(&format!("/proc/{pid}")).exists()
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The url already serving `effective`'s id (its kind and model), read
/// from the AGL-064 runtime files (AGL-120, D5): the registry of servers
/// agl started, keyed by pid. A live `<pid>.json` whose kind and model
/// match names the url mine; `None` when none does — the engine I need is
/// not up, so `Autostart` starts one at the next free port. The url of
/// `url` is skipped: that is the foreign one we came away from.
pub fn url_of_id(runtime: Option<&Path>, effective: &ServerFile, away: &str) -> Option<String> {
    let dir = servers_dir(runtime?);
    let model = model_of(effective).ok()??;
    let kind = effective.kind().name();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let Ok(record) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let record: serde_json::Value = serde_json::from_str(&record).ok()?;
        // The process behind a file must still run (AGL-064, D4).
        let pid = record.get("pid")?.as_u64()?;
        if !Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        if record.get("kind")?.as_str()? != kind || record.get("model")?.as_str()? != model {
            continue;
        }
        let url = record.get("url")?.as_str()?;
        if url != away {
            return Some(url.to_owned());
        }
    }
    None
}

/// The url a session runs on (AGL-120, D5): when a live registry file
/// names a server of my id already up, its url; else the configured one,
/// which the normal flow probes and starts as it does today. An external
/// server (AGL-052, D4) is left outside the registry's care.
pub fn rebind_url(runtime: Option<&Path>, effective: &ServerFile) -> Option<String> {
    if effective.external() {
        return None;
    }
    url_of_id(runtime, effective, &effective.url())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::engine::tests::model;

    #[test]
    fn the_registry_names_the_url_of_its_id() {
        // AGL-120, D5: a live `servers/<pid>.json` of my kind and model
        // names the url already serving it; a foreign url is skipped, a
        // dead pid's file ignored, a different model's file not mine.
        let runtime = runtime_dir("registry-url");
        let servers = runtime.join("servers");
        fs::create_dir_all(&servers).unwrap();
        let mine = model("m.gguf", 4096);
        let other = model("other.gguf", 4096);
        // A running server of my model, at a port of its own.
        let pid = std::process::id();
        fs::write(
            servers.join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "kind": "llama", "url": "http://127.0.0.1:19001", "model": "m.gguf"}).to_string(),
        )
        .unwrap();
        // A different model: not mine.
        assert_eq!(url_of_id(Some(&runtime), &other, "http://127.0.0.1:18080"), None);
        // My model, away from a url that is not the recorded one: found.
        assert_eq!(
            url_of_id(Some(&runtime), &mine, "http://127.0.0.1:18080").as_deref(),
            Some("http://127.0.0.1:19001")
        );
        // Away from that very url: nothing else serves my id.
        assert_eq!(url_of_id(Some(&runtime), &mine, "http://127.0.0.1:19001"), None);
        fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn the_run_rebinds_to_the_registrys_url_of_its_id() {
        // AGL-120, D5: a live registry file of my id names the url to run
        // on; none, or a server outside agl's care, keeps the configured.
        let runtime = runtime_dir("rebind-url");
        let servers = runtime.join("servers");
        fs::create_dir_all(&servers).unwrap();
        let pid = std::process::id();
        fs::write(
            servers.join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "kind": "llama", "url": "http://127.0.0.1:19001", "model": "m.gguf"}).to_string(),
        )
        .unwrap();
        // My id, configured elsewhere: the registry's url wins.
        let mine = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            ..model("m.gguf", 4096)
        };
        assert_eq!(rebind_url(Some(&runtime), &mine).as_deref(), Some("http://127.0.0.1:19001"));
        // Another id: nothing to rebind to.
        let other = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            ..model("other.gguf", 4096)
        };
        assert_eq!(rebind_url(Some(&runtime), &other), None);
        // A server outside agl's care: left alone, the configured url.
        let outside = ServerFile {
            llama_external: Some(true),
            ..model("m.gguf", 4096)
        };
        assert_eq!(rebind_url(Some(&runtime), &outside), None);
        fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn the_record_is_written_read_and_removed() {
        let dir = std::env::temp_dir().join(format!("agentLIBRE-record-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let effective = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            model: Some("m.gguf".into()),
            context: Some(16384),
            args: Some(vec!["-ngl".into(), "999".into()]),
            ..ServerFile::default()
        };
        assert_eq!(read_record(&dir), None);
        // The record of a process that runs reads back as written.
        write_record(&dir, std::process::id(), &effective).unwrap();
        assert_eq!(read_record(&dir), Some(effective.clone()));
        // The record of a process that is gone is no record.
        write_record(&dir, u32::MAX, &effective).unwrap();
        assert_eq!(read_record(&dir), None);
        // A record that cannot be read is no record.
        fs::write(dir.join(STATE_FILE), "pid = \"x\"\n").unwrap();
        assert_eq!(read_record(&dir), None);
        write_record(&dir, std::process::id(), &effective).unwrap();
        remove_record(&dir, None, None);
        assert_eq!(read_record(&dir), None);
        assert!(!dir.join(STATE_FILE).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A fresh runtime directory for a test, below the temp dir.
    fn runtime_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentLIBRE-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn json_of(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn the_runtime_file_says_which_server_runs() {
        // AGL-064, D1, D2: `servers/<pid>.json` holds pid, kind, url, model.
        let runtime = runtime_dir("runtime-file");
        let llama = ServerFile {
            llama_url: Some("http://127.0.0.1:18080".into()),
            model: Some("/m/big.gguf".into()),
            context: Some(16384),
            ..ServerFile::default()
        };
        let pid = std::process::id();
        write_runtime_file(Some(&runtime), pid, &llama).unwrap();
        let path = runtime.join("servers").join(format!("{pid}.json"));
        assert_eq!(
            json_of(&path),
            serde_json::json!({
                "pid": pid,
                "kind": "llama",
                "url": "http://127.0.0.1:18080",
                "model": "/m/big.gguf",
            })
        );
        // Written whole to a temporary name and renamed: nothing else stays.
        let names: Vec<_> = fs::read_dir(runtime.join("servers"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(format!("{pid}.json"))]);
        // The kind and the url of the Strata server; the model is the
        // Function's `name` (AGL-089, D5).
        let strata = ServerFile {
            kind: Some("strata".into()),
            strata_url: Some("http://127.0.0.1:18090".into()),
            strata_name: Some("qwen".into()),
            ..ServerFile::default()
        };
        write_runtime_file(Some(&runtime), pid, &strata).unwrap();
        assert_eq!(
            json_of(&path),
            serde_json::json!({
                "pid": pid,
                "kind": "strata",
                "url": "http://127.0.0.1:18090",
                "model": "qwen",
            })
        );
        fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn the_runtime_files_of_gone_processes_are_removed_by_the_next_write_or_removal() {
        // AGL-064, D4: by `/proc/<pid>`, as `read_record` judges.
        let runtime = runtime_dir("runtime-sweep");
        let servers = runtime.join("servers");
        fs::create_dir_all(&servers).unwrap();
        let effective = model("m.gguf", 4096);
        let gone = servers.join(format!("{}.json", u32::MAX));
        let other = servers.join("notes.txt");
        fs::write(&gone, "{}").unwrap();
        fs::write(&other, "x").unwrap();
        let pid = std::process::id();
        write_runtime_file(Some(&runtime), pid, &effective).unwrap();
        assert!(!gone.exists());
        assert!(servers.join(format!("{pid}.json")).exists());
        // What is no server file is left alone.
        assert!(other.exists());
        // A removal sweeps too, and takes the file of the pid in hand,
        // not that of another process that runs.
        let parent = std::os::unix::process::parent_id();
        write_runtime_file(Some(&runtime), parent, &effective).unwrap();
        fs::write(&gone, "{}").unwrap();
        remove_runtime_file(Some(&runtime), Some(pid));
        assert!(!gone.exists());
        assert!(!servers.join(format!("{pid}.json")).exists());
        assert!(servers.join(format!("{parent}.json")).exists());
        fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn the_record_removed_takes_the_runtime_file_of_its_pid() {
        // AGL-064, D3: the pid from `server.state`, or the one in hand.
        let runtime = runtime_dir("runtime-record");
        let dir = runtime.join("store");
        fs::create_dir_all(&dir).unwrap();
        let effective = model("m.gguf", 4096);
        let pid = std::process::id();
        let parent = std::os::unix::process::parent_id();
        let file = |pid: u32| runtime.join("servers").join(format!("{pid}.json"));
        write_record(&dir, pid, &effective).unwrap();
        write_runtime_file(Some(&runtime), pid, &effective).unwrap();
        write_runtime_file(Some(&runtime), parent, &effective).unwrap();
        remove_record(&dir, Some(&runtime), None);
        assert!(!dir.join(STATE_FILE).exists());
        assert!(!file(pid).exists());
        assert!(file(parent).exists());
        // No record, the pid of the child in hand.
        remove_record(&dir, Some(&runtime), Some(parent));
        assert!(!file(parent).exists());
        fs::remove_dir_all(&runtime).unwrap();
    }

    #[test]
    fn without_a_runtime_directory_no_file_is_written_and_nothing_fails() {
        // AGL-064, D7.
        let effective = model("m.gguf", 4096);
        write_runtime_file(None, std::process::id(), &effective).unwrap();
        remove_runtime_file(None, Some(std::process::id()));
        remove_record(&std::env::temp_dir().join("agentLIBRE-no-such-store"), None, Some(1));
    }
}
