use super::*;

pub(super) struct ChatInput {
    receiver: mpsc::UnboundedReceiver<io::Result<Event>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ChatInput {
    pub(super) fn new() -> io::Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("agl-chat-input".to_owned())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    match crossterm::event::poll(CHAT_INPUT_POLL_INTERVAL) {
                        Ok(true) => match crossterm::event::read() {
                            Ok(event) => {
                                if sender.send(Ok(event)).is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = sender.send(Err(error));
                                break;
                            }
                        },
                        Ok(false) => {}
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            receiver,
            shutdown,
            thread: Some(thread),
        })
    }

    pub(super) async fn next(&mut self) -> Option<io::Result<Event>> {
        self.receiver.recv().await
    }
}

impl Drop for ChatInput {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ComposerSubmission {
    Prompt(String),
    Shell(String),
    SwitchTerminal,
    Command(String),
    Picker(PickerSubmit),
}

impl Composer {
    pub(super) fn submit(&mut self) -> Option<ComposerSubmission> {
        if self.buffer.trim().is_empty() {
            if self.mode == ComposerMode::Shell {
                self.reset();
                return Some(ComposerSubmission::SwitchTerminal);
            }
            return None;
        }
        if self.mode == ComposerMode::Shell {
            return Some(ComposerSubmission::Shell(self.buffer.clone()));
        }
        let text = self.buffer.trim_end_matches(['\r', '\n']).to_owned();
        let submission = match self.mode {
            ComposerMode::Prompt => ComposerSubmission::Prompt(text),
            ComposerMode::Command => ComposerSubmission::Command(text),
            ComposerMode::Shell => unreachable!("Shell submission returns without clearing"),
        };
        self.reset();
        Some(submission)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryRecord {
    schema: String,
    timestamp_unix_ms: u128,
    mode: String,
    input: String,
}

pub(super) struct InputHistory {
    pub(super) root: Option<PathBuf>,
    pub(super) prompt: Vec<String>,
}

impl InputHistory {
    pub(super) fn load(
        state_dir: &Path,
        workspace_history_scope: &str,
        enabled: bool,
    ) -> (Self, Vec<String>) {
        if !enabled {
            return (
                Self {
                    root: None,
                    prompt: Vec::new(),
                },
                Vec::new(),
            );
        }
        let digest = Sha256::digest(workspace_history_scope.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let root = state_dir.join("history").join(digest);
        let mut warnings = Vec::new();
        if let Err(error) = create_private_directory(&root) {
            warnings.push(format!("input history disabled: {error:#}"));
            return (
                Self {
                    root: None,
                    prompt: Vec::new(),
                },
                warnings,
            );
        }
        let prompt = read_history_file(&root.join("prompt.jsonl"), &mut warnings);
        (
            Self {
                root: Some(root),
                prompt,
            },
            warnings,
        )
    }

    pub(super) fn entries(&self, mode: ComposerMode) -> &[String] {
        match mode {
            ComposerMode::Prompt => &self.prompt,
            ComposerMode::Shell | ComposerMode::Command => &[],
        }
    }

    pub(super) fn record_prompt(&mut self, input: &str) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let entries = &mut self.prompt;
        if entries.last().is_some_and(|last| last == input) {
            return Ok(());
        }
        entries.push(input.to_owned());
        if entries.len() > MAX_HISTORY_ENTRIES {
            entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
        }
        let path = root.join("prompt.jsonl");
        let lock_path = root.join("prompt.lock");
        let lock = open_private_file(&lock_path, false)?;
        lock.lock_exclusive()
            .context("failed to lock input history")?;
        let record = HistoryRecord {
            schema: "agl-terminal.input_history.v1".to_owned(),
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            mode: "prompt".to_owned(),
            input: input.to_owned(),
        };
        let line = serde_json::to_vec(&record).context("failed to encode input history")?;
        let mut file = open_private_file(&path, true)?;
        file.write_all(&line)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        let metadata = file.metadata()?;
        drop(file);
        if metadata.len() as usize > MAX_HISTORY_BYTES {
            compact_history(&path, entries)?;
        }
        fs2::FileExt::unlock(&lock).context("failed to unlock input history")?;
        Ok(())
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("history path is not a private directory")
    }
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

fn open_private_file(path: &Path, append: bool) -> Result<std::fs::File> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        bail!("history target is not a regular file: {}", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(file)
}

fn read_history_file(path: &Path, warnings: &mut Vec<String>) -> Vec<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(format!("history read failed: {error}"));
            return Vec::new();
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() && metadata.len() <= (MAX_HISTORY_BYTES * 2) as u64 => {
            metadata
        }
        Ok(_) => {
            warnings.push(format!("history file is oversized: {}", path.display()));
            return Vec::new();
        }
        Err(error) => {
            warnings.push(format!("history metadata failed: {error}"));
            return Vec::new();
        }
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if let Err(error) = file.read_to_end(&mut bytes) {
        warnings.push(format!("history read failed: {error}"));
        return Vec::new();
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            warnings.push(format!("history is not UTF-8: {}", path.display()));
            return Vec::new();
        }
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.len() > MAX_COMPOSER_BYTES {
            warnings.push("oversized input history record skipped".to_owned());
            continue;
        }
        let Ok(record) = serde_json::from_str::<HistoryRecord>(line) else {
            warnings.push("corrupt input history record skipped".to_owned());
            continue;
        };
        if record.schema == "agl-terminal.input_history.v1"
            && record.mode == "prompt"
            && record.input.len() <= MAX_COMPOSER_BYTES
            && entries.last() != Some(&record.input)
        {
            entries.push(record.input);
        }
    }
    if entries.len() > MAX_HISTORY_ENTRIES {
        entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
    }
    entries
}

fn compact_history(path: &Path, entries: &[String]) -> Result<()> {
    let temporary = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
    let mut file = open_private_file(&temporary, false)?;
    file.set_len(0)?;
    for input in entries {
        let record = HistoryRecord {
            schema: "agl-terminal.input_history.v1".to_owned(),
            timestamp_unix_ms: 0,
            mode: "prompt".to_owned(),
            input: input.clone(),
        };
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}
