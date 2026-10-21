//! The whisper.cpp server of a project as a transient systemd user unit,
//! and the client that sends it an audio (AGL-014, D4, D17, D22): the unit
//! is `agentLIBRE-whisper-server-<key>`, started the way llama-server is.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::attach::Transcriber;
use crate::backend::llama::Llama;
use crate::cli::serve::{
    Autostart, Health, find_program, health, local_address, port_open, unit_active, wait_ready,
};
use crate::error::Error;
use crate::forge::Workspace;
use crate::i18n::{Lang, m};
use crate::media::{self, Source};
use crate::settings::WhisperFile;
use crate::tr;

/// The name every unit of a whisper server starts with (D22).
pub const UNIT_PREFIX: &str = "agentLIBRE-whisper-server";

/// The whisper server of the project: its unit and settings.
#[derive(Debug)]
pub struct WhisperServer {
    pub unit: String,
    pub url: String,
    pub model: String,
    pub args: Vec<String>,
}

impl WhisperServer {
    /// The server of `project`, with `settings` the project's `[whisper]`
    /// over the user's. A project that is not registered has no unit
    /// (AGL-015, D14); `url` and `model` must be set.
    pub fn of(
        project: &Workspace,
        settings: &WhisperFile,
        user_path: &Path,
    ) -> Result<WhisperServer, Error> {
        let key = project.unit_key().ok_or_else(|| Error::ProjectNotRegistered {
            project: project.project.clone(),
        })?;
        let (Some(url), Some(model)) = (&settings.url, &settings.model) else {
            return Err(Error::WhisperNotSet {
                user_path: user_path.into(),
                project_path: project.dir.join(crate::forge::MARKER_FILE),
            });
        };
        Ok(WhisperServer {
            unit: format!("{UNIT_PREFIX}-{key}"),
            url: url.clone(),
            model: model.clone(),
            args: settings.args.clone().unwrap_or_default(),
        })
    }
}

/// The arguments of `systemd-run` for the unit: whisper-server with the
/// model, the address, the language auto-detected (D22), then the
/// settings' `args`, which may set another language.
fn systemd_run_args(
    server: &WhisperServer,
    program: &Path,
    model: &Path,
    host: &str,
    port: u16,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--user",
        &format!("--unit={}", server.unit),
        "--collect",
        "--quiet",
        "--",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(program.into());
    args.push("-m".into());
    args.push(model.into());
    for arg in ["--host", host, "--port", &port.to_string(), "-l", "auto"] {
        args.push(arg.into());
    }
    args.extend(server.args.iter().map(OsString::from));
    args
}

/// Brings the whisper server up when it does not answer (D10 of AGL-015).
pub fn ensure(
    server: &WhisperServer,
    models: &Path,
    path_var: Option<OsString>,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<(), Error> {
    let probe = Llama::new(&server.url);
    match health(&probe) {
        Health::Ready => return Ok(()),
        Health::Loading => return wait_ready(&probe, None, &m::WHISPER_READY, say, lang),
        Health::Down => {}
    }
    if unit_active(&server.unit)? {
        return wait_ready(&probe, Some(&server.unit), &m::WHISPER_READY, say, lang);
    }
    let (host, port) = local_address(&server.url).ok_or_else(|| Error::NotLocalServer {
        url: server.url.clone(),
    })?;
    if port_open(&host, port) {
        return Err(Error::PortBusy {
            url: server.url.clone(),
            units: Vec::new(),
        });
    }
    let model = models.join(&server.model);
    if !model.is_file() {
        return Err(Error::ModelNotFound { path: model });
    }
    let program = find_program("whisper-server", path_var).ok_or(Error::WhisperServerNotFound)?;
    let output = Command::new("systemd-run")
        .args(systemd_run_args(server, &program, &model, &host, port))
        .stdin(Stdio::null())
        .output()
        .map_err(|source| Error::Systemd {
            detail: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(Error::Systemd {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    say(&tr!(
        lang,
        m::WHISPER_STARTING,
        model = server.model,
        url = server.url,
        unit = server.unit
    ));
    wait_ready(&probe, Some(&server.unit), &m::WHISPER_READY, say, lang)
}

/// The text of `wav` (16 kHz mono) from the whisper server at `url`.
pub fn transcribe(url: &str, wav: &[u8]) -> Result<String, Error> {
    let boundary = "----agentLIBREwhisper7f3a9c";
    let mut body: Vec<u8> = Vec::new();
    for (name, value) in [("response_format", "json"), ("temperature", "0.0")] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let fail = |detail: String| Error::Transcribe {
        url: url.into(),
        detail,
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .build()
        .into();
    let mut response = agent
        .post(format!("{}/inference", url.trim_end_matches('/')))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .send(&body[..])
        .map_err(|error| fail(error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| fail(error.to_string()))?;
    if status != 200 {
        return Err(fail(format!("{status}: {}", text.trim())));
    }
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|error| fail(error.to_string()))?;
    if let Some(error) = json["error"].as_str() {
        return Err(fail(error.into()));
    }
    json["text"]
        .as_str()
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .ok_or_else(|| fail("no `text` in the reply".into()))
}

/// What the REPL's voice input needs of whisper (AGL-016).
pub trait Dictation: Send + Sync {
    /// Whether whisper is set up for this project; the error says what is
    /// missing, before the human speaks.
    fn ready(&self) -> Result<(), Error>;
    /// Brings the server up; returns when it answers (D6).
    fn warm_up(&self, say: &mut dyn FnMut(&str)) -> Result<(), Error>;
    /// The text of a 16 kHz mono WAV.
    fn transcribe(&self, wav: &[u8], say: &mut dyn FnMut(&str)) -> Result<String, Error>;
}

/// The whisper server of a project, owned so that the REPL's input thread
/// can hold it (AGL-016).
pub struct Voice {
    project: Workspace,
    settings: WhisperFile,
    user_path: PathBuf,
    models: PathBuf,
    path_var: Option<OsString>,
    lang: Lang,
}

impl Voice {
    /// The server `autostart` brings up for audio, as an owned value.
    pub fn of(autostart: &Autostart) -> Voice {
        Voice {
            project: autostart.project.clone(),
            settings: autostart.whisper.clone(),
            user_path: autostart.user_path.clone(),
            models: autostart.models.clone(),
            path_var: autostart.path_var.clone(),
            lang: autostart.lang,
        }
    }

    fn server(&self) -> Result<WhisperServer, Error> {
        WhisperServer::of(&self.project, &self.settings, &self.user_path)
    }
}

impl Dictation for Voice {
    fn ready(&self) -> Result<(), Error> {
        self.server().map(|_| ())
    }

    fn warm_up(&self, say: &mut dyn FnMut(&str)) -> Result<(), Error> {
        let server = self.server()?;
        ensure(&server, &self.models, self.path_var.clone(), say, self.lang)
    }

    fn transcribe(&self, wav: &[u8], say: &mut dyn FnMut(&str)) -> Result<String, Error> {
        let server = self.server()?;
        ensure(&server, &self.models, self.path_var.clone(), say, self.lang)?;
        transcribe(&server.url, wav)
    }
}

impl Transcriber for Autostart<'_> {
    fn transcribe(&self, source: &Source, say: &mut dyn FnMut(&str)) -> Result<String, Error> {
        let server = WhisperServer::of(self.project, self.whisper, &self.user_path)?;
        ensure(&server, &self.models, self.path_var.clone(), say, self.lang)?;
        say(&tr!(self.lang, m::TRANSCRIBING, name = source.name));
        let wav = media::audio_to_wav(&source.bytes)?;
        transcribe(&server.url, &wav)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(whisper: WhisperFile) -> (Workspace, WhisperFile) {
        let origin = format!("sha256:60af300abea6{}", "0".repeat(52));
        (
            Workspace {
                project: PathBuf::from("/p/agl-lite"),
                identity: "sha256:aa".into(),
                key: "agl-lite-60af300abea6".into(),
                dir: PathBuf::from("/d/agl-lite-60af300abea6"),
                function: None,
                server: Default::default(),
                whisper: WhisperFile::default(),
                origin: Some(origin),
            },
            whisper,
        )
    }

    #[test]
    fn the_unit_is_named_by_the_project_and_needs_url_and_model() {
        let (ws, settings) = registered(WhisperFile {
            url: Some("http://127.0.0.1:18081".into()),
            model: Some("w.bin".into()),
            args: Some(vec!["-t".into(), "12".into()]),
        });
        let server = WhisperServer::of(&ws, &settings, Path::new("/c/agentLIBRE.toml")).unwrap();
        assert_eq!(server.unit, "agentLIBRE-whisper-server-60af300abea6");
        assert_eq!(server.args, ["-t", "12"]);
        let (ws, partial) = registered(WhisperFile {
            url: Some("http://127.0.0.1:18081".into()),
            ..WhisperFile::default()
        });
        assert!(matches!(
            WhisperServer::of(&ws, &partial, Path::new("/c/agentLIBRE.toml")),
            Err(Error::WhisperNotSet { .. })
        ));
        // Not registered: no unit, as for llama-server (AGL-015, D14).
        let (mut ws, settings) = registered(WhisperFile::default());
        ws.origin = None;
        assert!(matches!(
            WhisperServer::of(&ws, &settings, Path::new("/c/agentLIBRE.toml")),
            Err(Error::ProjectNotRegistered { .. })
        ));
    }

    #[test]
    fn the_voice_input_is_ready_only_when_whisper_is_set() {
        let (ws, settings) = registered(WhisperFile::default());
        let mut voice = Voice {
            project: ws,
            settings,
            user_path: PathBuf::from("/c/agentLIBRE.toml"),
            models: PathBuf::from("/m"),
            path_var: None,
            lang: Lang::En,
        };
        assert!(matches!(voice.ready(), Err(Error::WhisperNotSet { .. })));
        voice.settings = WhisperFile {
            url: Some("http://127.0.0.1:18081".into()),
            model: Some("w.bin".into()),
            args: None,
        };
        assert!(voice.ready().is_ok());
    }

    #[test]
    fn the_unit_runs_whisper_server_with_the_language_auto_and_the_args_after() {
        let server = WhisperServer {
            unit: "agentLIBRE-whisper-server-60af300abea6".into(),
            url: "http://127.0.0.1:18081".into(),
            model: "w.bin".into(),
            args: vec!["-t".into(), "12".into(), "-l".into(), "uk".into()],
        };
        let args = systemd_run_args(
            &server,
            Path::new("/bin/whisper-server"),
            Path::new("/m/w.bin"),
            "127.0.0.1",
            18081,
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user",
                "--unit=agentLIBRE-whisper-server-60af300abea6",
                "--collect",
                "--quiet",
                "--",
                "/bin/whisper-server",
                "-m",
                "/m/w.bin",
                "--host",
                "127.0.0.1",
                "--port",
                "18081",
                "-l",
                "auto",
                "-t",
                "12",
                "-l",
                "uk"
            ]
        );
    }

    /// A one-request HTTP server that answers with `reply`; returns the
    /// URL and what the request was.
    fn one_reply(status: u16, reply: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            // Read the head, then as much body as the length says.
            loop {
                let n = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| line.to_lowercase().strip_prefix("content-length: ").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            request
        });
        (url, handle)
    }

    #[test]
    fn the_audio_goes_as_a_multipart_form_and_the_text_comes_back_trimmed() {
        let (url, handle) = one_reply(200, "{\"text\":\" And so my fellow Americans,\\n ask not\\n\"}");
        let text = transcribe(&url, b"RIFFwav").unwrap();
        assert_eq!(text, "And so my fellow Americans, ask not");
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(request.starts_with("POST /inference HTTP/1.1"), "{request}");
        assert!(request.contains("multipart/form-data; boundary="));
        assert!(request.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(request.contains("name=\"response_format\"\r\n\r\njson"));
        assert!(request.contains("RIFFwav"));
    }

    #[test]
    fn a_whisper_error_is_an_error_with_its_text() {
        let (url, handle) = one_reply(500, "{\"error\":\"boom\"}");
        let error = transcribe(&url, b"x").unwrap_err();
        handle.join().unwrap();
        assert!(matches!(&error, Error::Transcribe { detail, .. } if detail.contains("500") && detail.contains("boom")));
        let (url, handle) = one_reply(200, "{\"error\":\"no audio\"}");
        let error = transcribe(&url, b"x").unwrap_err();
        handle.join().unwrap();
        assert!(matches!(&error, Error::Transcribe { detail, .. } if detail == "no audio"));
    }
}
