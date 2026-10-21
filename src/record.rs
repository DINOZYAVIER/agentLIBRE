//! Recording the human's voice from the microphone (AGL-016): `ffmpeg`
//! reads the system's default source through the Pulse layer of PipeWire
//! and writes 16 kHz mono WAV, which whisper reads as it is (D4).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::Error;

/// How long a recording that is stopped is waited for to finish its file.
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a new recording is watched for an immediate failure (no
/// source, no server of sound).
const START_WATCH: Duration = Duration::from_millis(250);

/// A recording in progress.
pub trait Recording: Send {
    /// Whether it ended by itself: the limit was reached, or `ffmpeg`
    /// stopped.
    fn finished(&mut self) -> bool;
    /// Ends it and returns the WAV.
    fn stop(self: Box<Self>) -> Result<Vec<u8>, Error>;
    /// Ends it and keeps nothing.
    fn cancel(self: Box<Self>);
}

/// Starts recording the default source for at most `limit` seconds (D4,
/// D5).
pub fn start(limit: u32) -> Result<Box<dyn Recording>, Error> {
    Ffmpeg::start("ffmpeg".as_ref(), limit).map(|recording| Box::new(recording) as Box<dyn Recording>)
}

/// The arguments of `ffmpeg` for a recording of `limit` seconds into
/// `path`.
fn ffmpeg_args(limit: u32, path: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "-v", "error", "-y", "-f", "pulse", "-i", "default", "-ar", "16000", "-ac", "1", "-t",
    ]
    .iter()
    .map(Into::into)
    .collect();
    args.push(limit.to_string().into());
    args.push(path.into());
    args
}

struct Ffmpeg {
    child: Option<Child>,
    path: PathBuf,
}

impl Ffmpeg {
    fn start(program: &std::ffi::OsStr, limit: u32) -> Result<Ffmpeg, Error> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agentLIBRE-voice-{}-{n}.wav",
            std::process::id()
        ));
        let child = Command::new(program)
            .args(ffmpeg_args(limit, &path))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Ffmpeg {
                detail: source.to_string(),
            })?;
        let mut recording = Ffmpeg {
            child: Some(child),
            path,
        };
        // No source or no sound server: ffmpeg fails at once, and the
        // human is told before speaking.
        let watched = Instant::now();
        while watched.elapsed() < START_WATCH {
            if recording.exited().is_some() {
                return Err(Error::Ffmpeg {
                    detail: recording.failure(),
                });
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(recording)
    }

    /// The exit status when the process has ended.
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().ok().flatten()
    }

    /// What `ffmpeg` said, after it failed.
    fn failure(&mut self) -> String {
        use std::io::Read;
        let mut text = String::new();
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut text);
            }
        }
        let _ = fs::remove_file(&self.path);
        let text = text.trim();
        if text.is_empty() {
            "ffmpeg stopped without a recording".into()
        } else {
            text.into()
        }
    }
}

impl Recording for Ffmpeg {
    fn finished(&mut self) -> bool {
        self.child.is_none() || self.exited().is_some()
    }

    fn stop(mut self: Box<Self>) -> Result<Vec<u8>, Error> {
        let Some(mut child) = self.child.take() else {
            return Err(Error::Ffmpeg {
                detail: "ffmpeg stopped without a recording".into(),
            });
        };
        // SIGINT lets ffmpeg finish the file; one that ended by itself
        // at the limit is done already.
        if child.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
        }
        let waited = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if waited.elapsed() < FINISH_TIMEOUT => {
                    thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let bytes = fs::read(&self.path);
        let _ = fs::remove_file(&self.path);
        match (status, bytes) {
            (_, Ok(bytes)) if bytes.len() > 44 => Ok(bytes),
            _ => Err(Error::Ffmpeg {
                detail: "ffmpeg stopped without a recording".into(),
            }),
        }
    }

    fn cancel(mut self: Box<Self>) {
        self.kill();
    }
}

impl Ffmpeg {
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for Ffmpeg {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A directory with a script that stands for `ffmpeg`.
    struct Fake(PathBuf);

    impl Fake {
        fn new(name: &str, body: &str) -> Fake {
            let dir = std::env::temp_dir().join(format!("agentLIBRE-record-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let script = dir.join("ffmpeg");
            fs::write(&script, format!("#!/bin/sh\nfor last; do :; done\n{body}\n")).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            Fake(script)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    /// Starts the fake. A script that was just written may be busy for a
    /// moment, when another test forks while its file is open; it is tried
    /// again.
    fn start_fake(fake: &Fake, limit: u32) -> Result<Ffmpeg, Error> {
        let mut tries = 0;
        loop {
            match Ffmpeg::start(fake.0.as_os_str(), limit) {
                Err(Error::Ffmpeg { detail }) if detail.contains("busy") && tries < 50 => {
                    tries += 1;
                    thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
    }

    /// A WAV long enough for `stop` to take it for a recording.
    const WRITE: &str = "head -c 100 /dev/zero > \"$last\"";

    #[test]
    fn the_command_records_the_default_source_as_16_khz_mono() {
        let args = ffmpeg_args(60, Path::new("/t/v.wav"));
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            ["-v", "error", "-y", "-f", "pulse", "-i", "default", "-ar", "16000", "-ac", "1", "-t", "60", "/t/v.wav"]
        );
    }

    #[test]
    fn stop_finishes_the_file_and_returns_it() {
        // Writes on SIGINT, as ffmpeg finishes its file.
        let fake = Fake::new("stop", &format!("trap '{WRITE}; exit 0' INT\nwhile true; do sleep 0.02; done"));
        let mut recording = start_fake(&fake, 60).unwrap();
        assert!(!recording.finished());
        let path = recording.path.clone();
        let bytes = Box::new(recording).stop().unwrap();
        assert_eq!(bytes.len(), 100);
        assert!(!path.exists());
    }

    #[test]
    fn cancel_keeps_nothing() {
        let fake = Fake::new("cancel", &format!("{WRITE}\nwhile true; do sleep 0.02; done"));
        let recording = start_fake(&fake, 60).unwrap();
        let path = recording.path.clone();
        assert!(path.exists());
        Box::new(recording).cancel();
        assert!(!path.exists());
    }

    #[test]
    fn a_recording_that_ends_at_the_limit_is_finished_and_stops_with_its_file() {
        let fake = Fake::new("limit", &format!("sleep 0.4\n{WRITE}"));
        let mut recording = start_fake(&fake, 1).unwrap();
        let waited = Instant::now();
        while !recording.finished() {
            assert!(waited.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(Box::new(recording).stop().unwrap().len(), 100);
    }

    #[test]
    fn an_ffmpeg_that_fails_at_once_says_why_and_a_missing_one_is_an_error() {
        let fake = Fake::new("fail", "echo 'default: Connection refused' >&2\nexit 1");
        let error = start_fake(&fake, 60).map(|_| ()).unwrap_err();
        assert!(matches!(&error, Error::Ffmpeg { detail } if detail.contains("Connection refused")));
        let missing = Ffmpeg::start("/no/such/ffmpeg".as_ref(), 60).map(|_| ()).unwrap_err();
        assert!(matches!(missing, Error::Ffmpeg { .. }));
    }

    #[test]
    fn dropping_a_recording_stops_the_process_and_removes_the_file() {
        let fake = Fake::new("drop", &format!("{WRITE}\nwhile true; do sleep 0.02; done"));
        let recording = start_fake(&fake, 60).unwrap();
        let path = recording.path.clone();
        drop(recording);
        assert!(!path.exists());
    }
}
