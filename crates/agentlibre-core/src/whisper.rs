//! The whisper.cpp server of a project as a transient systemd user unit,
//! and the client that sends it an audio (AGL-014, D4, D17, D22): the unit
//! is `agentLIBRE-whisper-server-<key>`, started the way llama-server is. A
//! dictation goes to the model instead when it hears (AGL-051).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::attach::Transcriber;
use crate::backend::Backend;
use crate::backend::llama::Llama;
use crate::engine::{
    Autostart, Health, certificate_refused, find_program, health, local_address, port_open,
    unit_active, wait_ready,
};
use crate::error;
use crate::forge::Workspace;
use crate::i18n::{Lang, Msg, m};
use crate::media::{self, Source};
use crate::settings::{ServerKind, WhisperFile};
use crate::tr;

/// What can go wrong with the whisper server (AGL-014, D22; AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// An audio needs whisper and `whisper` lacks `url` or `model`
    /// (AGL-014, D22).
    NotSet { user_path: PathBuf, project_path: PathBuf },
    /// The whisper server answered an error or nothing.
    Transcribe { url: String, detail: String },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NotSet { user_path, project_path } => tr!(
                lang,
                NOT_SET,
                user_path = user_path.display(),
                project_path = project_path.display()
            ),
            Self::Transcribe { url, detail } => {
                tr!(lang, TRANSCRIBE, url = url, detail = detail)
            }
        }
    }
}

/// `Error::NotSet`.
const NOT_SET: Msg = Msg {
    en: "an audio needs the whisper server, and `url` and `model` of `whisper` are not both set; add to {project_path} (this project) or {user_path} (the default of every project):\n\nwhisper = {{ url = \"http://127.0.0.1:18081\"; model = \"ggml-large-v3-q8_0.bin\"; }};",
    uk: "аудіо потребує whisper-сервера, а `url` і `model` у `whisper` задано не обидва; додай у {project_path} (цей проєкт) або {user_path} (типове для всіх проєктів):\n\nwhisper = {{ url = \"http://127.0.0.1:18081\"; model = \"ggml-large-v3-q8_0.bin\"; }};",
};

/// `Error::Transcribe`.
const TRANSCRIBE: Msg = Msg {
    en: "the whisper server at {url} could not transcribe: {detail}",
    uk: "whisper-сервер за {url} не зміг розпізнати: {detail}",
};

/// The name every unit of a whisper server starts with (D22).
pub const UNIT_PREFIX: &str = "agentLIBRE-whisper-server";

/// The whisper server of the project: its unit and settings.
#[derive(Debug)]
pub struct WhisperServer {
    pub unit: String,
    pub url: String,
    /// The certificate of the CA of an `https://` server (AGL-058, D12).
    pub private_ca: Option<String>,
    pub model: String,
    pub args: Vec<String>,
    /// The server binary by path (AGL-122, D9); `None` — `whisper-server`
    /// found in PATH.
    pub program: Option<String>,
    /// The language the server heard on the first request (AGL-122): from
    /// then on it goes with every request, and whisper stops hearing it
    /// over again — the detection costs the human three seconds a time.
    pub heard: std::sync::OnceLock<String>,
}

impl WhisperServer {
    /// The server of `project`, with `settings` the project's `[whisper]`
    /// over the user's. A project that is not registered has no unit
    /// (AGL-015, D14); `url` and `model` must be set.
    pub fn of(
        project: &Workspace,
        settings: &WhisperFile,
        user_path: &Path,
    ) -> Result<WhisperServer, error::Error> {
        let key = project.unit_key().ok_or_else(|| error::Error::from(crate::engine::error::Error::ProjectNotRegistered {
            project: project.project.clone(),
        }))?;
        let (Some(url), Some(model)) = (&settings.url, &settings.model) else {
            return Err(error::Error::Whisper(Error::NotSet {
                user_path: user_path.into(),
                project_path: project.dir.join(crate::settings::SETTINGS_FILE),
            }));
        };
        Ok(WhisperServer {
            unit: format!("{UNIT_PREFIX}-{key}"),
            url: url.clone(),
            private_ca: settings.private_ca.clone(),
            model: model.clone(),
            args: settings.args.clone().unwrap_or_default(),
            program: None,
            heard: std::sync::OnceLock::new(),
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
) -> Result<(), error::Error> {
    let probe =
        Llama::with_ca(&server.url, server.private_ca.as_deref())?.naming_ca("whisper.private_ca");
    match health(&probe) {
        Health::Ready => return Ok(()),
        Health::Loading => return wait_ready(&probe, None, "whisper-server", say, lang),
        // A remote whisper whose certificate is not trusted says so, not
        // that it is not local (AGL-121, D4).
        Health::Down => {
            if let Some(error) = certificate_refused(&probe) {
                return Err(error);
            }
        }
    }
    if unit_active(&server.unit)? {
        return wait_ready(&probe, Some(&server.unit), "whisper-server", say, lang);
    }
    let (host, port) = local_address(&server.url).ok_or_else(|| error::Error::from(crate::engine::error::Error::NotLocalServer {
        url: server.url.clone(),
    }))?;
    if port_open(&host, port) {
        return Err(crate::engine::error::Error::PortBusy {
            url: server.url.clone(),
            units: Vec::new(),
        }
        .into());
    }
    let model = models.join(&server.model);
    if !model.is_file() {
        return Err(crate::engine::error::Error::WeightsFileMissing {
            key: "whisper.model".into(),
            path: model,
        }
        .into());
    }
    let program = match &server.program {
        Some(p) => {
            let path = PathBuf::from(p);
            if !path.is_file() {
                return Err(crate::engine::error::Error::EngineNotFound { engine: p.clone() }.into());
            }
            path
        }
        None => find_program("whisper-server", path_var).ok_or_else(|| error::Error::from(crate::engine::error::Error::EngineNotFound {
            engine: "whisper-server".into(),
        }))?,
    };
    let output = Command::new("systemd-run")
        .args(systemd_run_args(server, &program, &model, &host, port))
        .stdin(Stdio::null())
        .output()
        .map_err(|source| error::Error::from(crate::engine::error::Error::Systemd {
            detail: source.to_string(),
        }))?;
    if !output.status.success() {
        return Err(crate::engine::error::Error::Systemd {
            detail: String::from_utf8_lossy(&output.stderr).trim().into(),
        }
        .into());
    }
    say(&tr!(
        lang,
        m::WHISPER_STARTING,
        model = server.model,
        url = server.url,
        unit = server.unit
    ));
    wait_ready(&probe, Some(&server.unit), "whisper-server", say, lang)
}

/// The text of `wav` (16 kHz mono) from the whisper server at `url`.
pub fn transcribe(
    url: &str,
    private_ca: Option<&str>,
    heard: &std::sync::OnceLock<String>,
    prompt: Option<&str>,
    wav: &[u8],
) -> Result<String, error::Error> {
    let boundary = "----agentLIBREwhisper7f3a9c";
    let mut body: Vec<u8> = Vec::new();
    // Until the server names its language once, ask for the verbose
    // answer that carries the name; every answer after that says it
    // up front and skips the three-second re-hearing (AGL-122).
    let language = heard.get();
    let mut fields: Vec<(&str, &str)> = vec![
        ("response_format", if language.is_some() { "json" } else { "verbose_json" }),
        ("temperature", "0.0"),
    ];
    if let Some(language) = language {
        fields.push(("language", language));
    }
    // The tail of what came before, so a piece cut from the middle of
    // a dictation keeps its punctuation and its words (AGL-122, D12).
    if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
        fields.push(("prompt", prompt));
    }
    for (name, value) in fields {
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
    let mut response = crate::backend::agent(url, private_ca)?
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
        return Err(fail(format!("{status}: {}", text.trim())).into());
    }
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|error| fail(error.to_string()))?;
    if let Some(error) = json["error"].as_str() {
        return Err(fail(error.into()).into());
    }
    if let Some(detected) = json["language"].as_str() {
        let _ = heard.set(detected.to_owned());
    }
    json["text"]
        .as_str()
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .ok_or_else(|| fail("no `text` in the reply".into())).map_err(Into::into)
}

/// What the REPL's voice input needs of whisper (AGL-016).
pub trait Dictation: Send + Sync {
    /// Whether whisper is set up for this project; the error says what is
    /// missing, before the human speaks.
    fn ready(&self) -> Result<(), error::Error>;
    /// Brings the server up; returns when it answers (D6).
    fn warm_up(&self, say: &mut dyn FnMut(&str)) -> Result<(), error::Error>;
    /// The text of a 16 kHz mono WAV, `prompt` the tail of the text
    /// that came before it when the WAV is a piece of a longer speech
    /// (AGL-122, D12).
    fn transcribe(
        &self,
        wav: &[u8],
        prompt: Option<&str>,
        say: &mut dyn FnMut(&str),
    ) -> Result<String, error::Error>;
}

/// Who turns a recording into text (AGL-051): the model of the Function when
/// it hears, whisper otherwise.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Model,
    /// The server of the Function still loads: whether it hears is not known.
    ModelLoading,
    Whisper,
}

/// The servers of a dictation, owned so that the REPL's input thread can hold
/// them (AGL-016, AGL-051).
pub struct Voice {
    /// The llama-server of the Function and its name for the human; the other
    /// kind does not take a recording.
    model: Option<(Llama, String)>,
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
        let model = (autostart.effective.kind() == ServerKind::Llama)
            .then(|| autostart.what().ok())
            .flatten()
            .and_then(|what| {
                let ca = autostart.effective.llama_private_ca.as_deref();
                Some((Llama::with_ca(autostart.llama.url(), ca).ok()?, what))
            });
        Voice {
            model,
            project: autostart.project.clone(),
            settings: autostart.whisper.clone(),
            user_path: autostart.user_path.clone(),
            models: autostart.models.clone(),
            path_var: autostart.path_var.clone(),
            lang: autostart.lang,
        }
    }

    fn server(&self) -> Result<WhisperServer, error::Error> {
        WhisperServer::of(&self.project, &self.settings, &self.user_path)
    }

    fn route(&self) -> Result<Route, error::Error> {
        let Some((model, _)) = &self.model else {
            return Ok(Route::Whisper);
        };
        Ok(match health(model) {
            Health::Ready if model.hears_audio()? => Route::Model,
            Health::Loading => Route::ModelLoading,
            Health::Ready | Health::Down => Route::Whisper,
        })
    }
}

impl Dictation for Voice {
    fn ready(&self) -> Result<(), error::Error> {
        match self.route()? {
            Route::Whisper => self.server().map(|_| ()),
            Route::Model | Route::ModelLoading => Ok(()),
        }
    }

    fn warm_up(&self, say: &mut dyn FnMut(&str)) -> Result<(), error::Error> {
        match (self.route()?, &self.model) {
            (Route::ModelLoading, Some((model, what))) => {
                wait_ready(model, None, what, say, self.lang)
            }
            (Route::Whisper, _) => {
                let server = self.server()?;
                ensure(&server, &self.models, self.path_var.clone(), say, self.lang)
            }
            _ => Ok(()),
        }
    }

    fn transcribe(
        &self,
        wav: &[u8],
        prompt: Option<&str>,
        say: &mut dyn FnMut(&str),
    ) -> Result<String, error::Error> {
        if let (Route::Model, Some((model, _))) = (self.route()?, &self.model) {
            return model.transcribe(wav);
        }
        let server = self.server()?;
        ensure(&server, &self.models, self.path_var.clone(), say, self.lang)?;
        transcribe(&server.url, server.private_ca.as_deref(), &server.heard, prompt, wav)
    }
}

impl Transcriber for Autostart {
    fn transcribe(&self, source: &Source, say: &mut dyn FnMut(&str)) -> Result<String, error::Error> {
        let server = WhisperServer::of(&self.project, &self.whisper, &self.user_path)?;
        ensure(&server, &self.models, self.path_var.clone(), say, self.lang)?;
        say(&tr!(self.lang, m::TRANSCRIBING, name = source.name));
        let wav = media::audio_to_wav(&source.bytes)?;
        transcribe(&server.url, server.private_ca.as_deref(), &server.heard, None, &wav)
    }
}

/// The name every unit of a draft server starts with (AGL-122, D9).
pub const DRAFT_PREFIX: &str = "agentLIBRE-draft-server";

/// A draft server that nobody feeds for this long goes away on its own
/// (AGL-122, D9): the fork's `--idle-exit`, spoken to the unit.
const DRAFT_IDLE: &str = "300";

/// The draft engine of the live dictation (AGL-122, D5, D9): a second
/// whisper-server, serving a Parakeet model, run as its own transient
/// unit and asked over the same `/inference` API as whisper.
pub struct Draft {
    pub server: WhisperServer,
    pub models: PathBuf,
    pub path_var: Option<OsString>,
}

impl Draft {
    /// `[whisper]`'s draft keys resolved to a server: `draft_url` and
    /// `draft_model` set, the model file there — or `None`, which means
    /// drafts are silently off. `key` names the unit, as the project's
    /// unit key names whisper's.
    pub fn of(
        settings: &WhisperFile,
        models: &Path,
        key: &str,
        path_var: Option<OsString>,
    ) -> Option<Draft> {
        let url = settings.draft_url.as_deref()?;
        let name = settings.draft_model.as_deref()?;
        let model =
            if Path::new(name).is_absolute() { PathBuf::from(name) } else { models.join(name) };
        if !model.is_file() {
            return None;
        }
        let mut args = settings.args.clone().unwrap_or_default();
        if !args.iter().any(|a| a == "--idle-exit") {
            args.extend(["--idle-exit".into(), DRAFT_IDLE.into()]);
        }
        Some(Draft {
            server: WhisperServer {
                unit: format!("{DRAFT_PREFIX}-{key}"),
                url: url.into(),
                private_ca: None,
                // The absolute path: `ensure` joins it with `models`,
                // and an absolute joins to itself.
                model: model.to_string_lossy().into_owned(),
                args,
                program: settings.draft_program.clone(),
                heard: std::sync::OnceLock::new(),
            },
            models: models.to_path_buf(),
            path_var,
        })
    }

    /// The draft text of a 16 kHz mono WAV chunk: bring the server up
    /// when it sleeps (the first chunk pays the load, D9), ask, return.
    pub fn transcribe(&self, wav: &[u8]) -> Result<String, error::Error> {
        ensure(&self.server, &self.models, self.path_var.clone(), &mut |_| {}, Lang::En)?;
        transcribe(&self.server.url, self.server.private_ca.as_deref(), &self.server.heard, None, wav)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
                search: Default::default(),
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
            private_ca: None,
            draft_model: None,
            draft_program: None,
            draft_url: None,
        });
        let server = WhisperServer::of(&ws, &settings, Path::new("/c/settings.nix")).unwrap();
        assert_eq!(server.unit, "agentLIBRE-whisper-server-60af300abea6");
        assert_eq!(server.args, ["-t", "12"]);
        let (ws, partial) = registered(WhisperFile {
            url: Some("http://127.0.0.1:18081".into()),
            ..WhisperFile::default()
        });
        assert!(matches!(
            WhisperServer::of(&ws, &partial, Path::new("/c/settings.nix")),
            Err(error::Error::Whisper(Error::NotSet { .. }))
        ));
        // Not registered: no unit, as for llama-server (AGL-015, D14).
        let (mut ws, settings) = registered(WhisperFile::default());
        ws.origin = None;
        assert!(matches!(
            WhisperServer::of(&ws, &settings, Path::new("/c/settings.nix")),
            Err(error::Error::Engine(crate::engine::error::Error::ProjectNotRegistered { .. }))
        ));
    }

    #[test]
    fn the_voice_input_is_ready_only_when_whisper_is_set() {
        let (ws, settings) = registered(WhisperFile::default());
        let mut voice = Voice {
            model: None,
            project: ws,
            settings,
            user_path: PathBuf::from("/c/settings.nix"),
            models: PathBuf::from("/m"),
            path_var: None,
            lang: Lang::En,
        };
        assert!(matches!(voice.ready(), Err(error::Error::Whisper(Error::NotSet { .. }))));
        voice.settings = WhisperFile {
            url: Some("http://127.0.0.1:18081".into()),
            model: Some("w.bin".into()),
            args: None,
            draft_model: None,
            draft_program: None,
            draft_url: None,
            private_ca: None,
        };
        assert!(voice.ready().is_ok());
    }

    /// A llama-server that answers `/health` with `status`, `/props` with
    /// `audio`, and the transcription with `heard`, and keeps the request.
    fn model_server(status: u16, audio: bool, heard: &'static str) -> (String, Arc<Mutex<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let kept = Arc::new(Mutex::new(String::new()));
        let keep = kept.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = Vec::new();
                let mut buffer = [0u8; 65536];
                loop {
                    let n = stream.read(&mut buffer).unwrap_or(0);
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request);
                    let Some(head) = text.find("\r\n\r\n") else {
                        if n == 0 { break } else { continue }
                    };
                    let length = text[..head]
                        .lines()
                        .find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if n == 0 || request.len() >= head + 4 + length {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let (code, body) = if text.starts_with("GET /health") {
                    (status, "{}".to_owned())
                } else if text.starts_with("GET /props") {
                    (200, format!("{{\"modalities\":{{\"audio\":{audio}}}}}"))
                } else {
                    *keep.lock().unwrap() = text;
                    (200, format!("{{\"choices\":[{{\"message\":{{\"content\":\"{heard}\"}}}}]}}"))
                };
                let _ = write!(stream, "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        (url, kept)
    }

    fn voice_of(url: Option<&str>, settings: WhisperFile) -> Voice {
        let (ws, settings) = registered(settings);
        Voice {
            model: url.map(|url| (Llama::new(url), "the model".to_owned())),
            project: ws,
            settings,
            user_path: PathBuf::from("/c/settings.nix"),
            models: PathBuf::from("/m"),
            path_var: None,
            lang: Lang::En,
        }
    }

    #[test]
    fn a_model_that_hears_takes_the_dictation_with_no_whisper_set() {
        let (url, kept) = model_server(200, true, "words");
        let voice = voice_of(Some(&url), WhisperFile::default());
        assert_eq!(voice.route().unwrap(), Route::Model);
        assert!(voice.ready().is_ok());
        assert!(voice.warm_up(&mut |_| {}).is_ok());
        assert_eq!(voice.transcribe(b"RIFF", None, &mut |_| {}).unwrap(), "words");
        let request = kept.lock().unwrap().clone();
        assert!(request.starts_with("POST /v1/chat/completions"), "{request}");
        assert!(request.contains("input_audio") && request.contains(&crate::media::base64(b"RIFF")));
    }

    #[test]
    fn a_model_that_does_not_hear_leaves_the_dictation_to_whisper() {
        let (url, _) = model_server(200, false, "");
        let voice = voice_of(Some(&url), WhisperFile::default());
        assert_eq!(voice.route().unwrap(), Route::Whisper);
        assert!(matches!(voice.ready(), Err(error::Error::Whisper(Error::NotSet { .. }))));
        // The other kind of server, and one that is down, the same.
        let voice = voice_of(None, WhisperFile::default());
        assert_eq!(voice.route().unwrap(), Route::Whisper);
        let voice = voice_of(Some("http://127.0.0.1:9"), WhisperFile::default());
        assert_eq!(voice.route().unwrap(), Route::Whisper);
    }

    #[test]
    fn a_server_that_still_loads_lets_the_human_speak() {
        let (url, _) = model_server(503, true, "");
        let voice = voice_of(Some(&url), WhisperFile::default());
        assert_eq!(voice.route().unwrap(), Route::ModelLoading);
        assert!(voice.ready().is_ok());
    }

    #[test]
    fn the_unit_runs_whisper_server_with_the_language_auto_and_the_args_after() {
        let server = WhisperServer {
            unit: "agentLIBRE-whisper-server-60af300abea6".into(),
            url: "http://127.0.0.1:18081".into(),
            private_ca: None,
            model: "w.bin".into(),
            args: vec!["-t".into(), "12".into(), "-l".into(), "uk".into()],
            program: None,
            heard: std::sync::OnceLock::new(),
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
        let text = transcribe(&url, None, &std::sync::OnceLock::new(), None, b"RIFFwav").unwrap();
        assert_eq!(text, "And so my fellow Americans, ask not");
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(request.starts_with("POST /inference HTTP/1.1"), "{request}");
        assert!(request.contains("multipart/form-data; boundary="));
        assert!(request.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(request.contains("name=\"response_format\"\r\n\r\nverbose_json"));
        assert!(request.contains("RIFFwav"));
    }

    #[test]
    fn a_heard_language_goes_with_every_request_afterwards() {
        // AGL-122: the server names its language once, every request
        // after that says it up front and saves the three-second
        // re-hearing.
        let heard = std::sync::OnceLock::new();
        let (url, handle) = one_reply(200, "{\"text\":\" hi\",\"language\":\"russian\"}");
        transcribe(&url, None, &heard, None, b"x").unwrap();
        handle.join().unwrap();
        assert_eq!(heard.get().map(String::as_str), Some("russian"));
        let (url, handle) = one_reply(200, "{\"text\":\" again\"}");
        transcribe(&url, None, &heard, None, b"x").unwrap();
        let request = String::from_utf8_lossy(&handle.join().unwrap()).to_string();
        assert!(request.contains("name=\"language\"\r\n\r\nrussian"), "{request}");
        assert!(request.contains("name=\"response_format\"\r\n\r\njson"));
    }

    #[test]
    fn a_whisper_error_is_an_error_with_its_text() {
        let (url, handle) = one_reply(500, "{\"error\":\"boom\"}");
        let error = transcribe(&url, None, &std::sync::OnceLock::new(), None, b"x").unwrap_err();
        handle.join().unwrap();
        assert!(matches!(&error, error::Error::Whisper(Error::Transcribe { detail, .. }) if detail.contains("500") && detail.contains("boom")));
        let (url, handle) = one_reply(200, "{\"error\":\"no audio\"}");
        let error = transcribe(&url, None, &std::sync::OnceLock::new(), None, b"x").unwrap_err();
        handle.join().unwrap();
        assert!(matches!(&error, error::Error::Whisper(Error::Transcribe { detail, .. }) if detail == "no audio"));
    }
}
