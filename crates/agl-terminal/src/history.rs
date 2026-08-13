use std::collections::VecDeque;
use std::fmt::{self, Debug, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use agl_exec::{ProcessError, ProcessErrorCode, Result};

use crate::{MAX_TYPED_TERMINAL_COMMAND_BYTES, TerminalId};

pub const MAX_HISTORY_COMMAND_BYTES: usize = MAX_TYPED_TERMINAL_COMMAND_BYTES;
pub const DEFAULT_HUMAN_HISTORY_ENTRIES: usize = 2_000;
pub const DEFAULT_HUMAN_HISTORY_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_AGENT_HISTORY_ENTRIES: usize = 256;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalHistoryOwner {
    HumanWorkspace { workspace_digest: String },
    EphemeralAgent { terminal_id: TerminalId },
}

#[derive(Clone)]
pub struct TerminalHistorySeed {
    commands: Vec<String>,
}

impl TerminalHistorySeed {
    pub fn empty() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn from_commands(commands: Vec<String>) -> Result<Self> {
        for command in &commands {
            validate_command(command)?;
        }
        Ok(Self { commands })
    }

    pub fn commands(&self) -> &[String] {
        &self.commands
    }
}

impl Debug for TerminalHistorySeed {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalHistorySeed")
            .field("command_count", &self.commands.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct HumanShellHistoryStore {
    root: PathBuf,
    max_entries: usize,
    max_bytes: usize,
}

impl HumanShellHistoryStore {
    pub fn new(root: PathBuf, max_entries: usize, max_bytes: usize) -> Result<Self> {
        if max_entries == 0 || max_bytes < MAX_HISTORY_COMMAND_BYTES {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "human shell history bounds must be nonzero and fit one command",
            ));
        }
        ensure_private_directory(&root)?;
        Ok(Self {
            root,
            max_entries,
            max_bytes,
        })
    }

    pub fn with_defaults(root: PathBuf) -> Result<Self> {
        Self::new(
            root,
            DEFAULT_HUMAN_HISTORY_ENTRIES,
            DEFAULT_HUMAN_HISTORY_BYTES,
        )
    }

    pub fn owner(&self, workspace_root: &Path) -> Result<TerminalHistoryOwner> {
        Ok(TerminalHistoryOwner::HumanWorkspace {
            workspace_digest: workspace_digest(workspace_root)?,
        })
    }

    pub fn load(&self, workspace_root: &Path) -> Result<TerminalHistorySeed> {
        let directory = self.workspace_directory(workspace_root)?;
        let lock = lock_history(&directory)?;
        let commands = read_history(
            &directory.join("history.jsonl"),
            self.max_entries,
            self.max_bytes,
        )?;
        unlock_history(lock)?;
        TerminalHistorySeed::from_commands(trim_history(commands, self.max_entries, self.max_bytes))
    }

    /// Stores exact Human shell input. No syntax-based secret inference is
    /// attempted: this store is private terminal data, never Chat history.
    pub fn append(&self, workspace_root: &Path, command: &str) -> Result<()> {
        validate_command(command)?;
        let directory = self.workspace_directory(workspace_root)?;
        let lock = lock_history(&directory)?;
        let result = (|| {
            let path = directory.join("history.jsonl");
            let mut commands = read_history(&path, self.max_entries, self.max_bytes)?;
            if commands.last().is_none_or(|last| last != command) {
                commands.push(command.to_owned());
            }
            let commands = trim_history(commands, self.max_entries, self.max_bytes);
            write_history_atomic(&directory, &path, &commands)
        })();
        let unlock = unlock_history(lock);
        result.and(unlock)
    }

    fn workspace_directory(&self, workspace_root: &Path) -> Result<PathBuf> {
        let directory = self.root.join(workspace_digest(workspace_root)?);
        ensure_private_directory(&directory)?;
        Ok(directory)
    }
}

#[derive(Debug)]
pub struct EphemeralTerminalHistory {
    terminal_id: TerminalId,
    max_entries: usize,
    commands: VecDeque<String>,
}

impl EphemeralTerminalHistory {
    pub fn new(terminal_id: TerminalId, max_entries: usize) -> Result<Self> {
        if max_entries == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "ephemeral terminal history bound must be nonzero",
            ));
        }
        Ok(Self {
            terminal_id,
            max_entries,
            commands: VecDeque::new(),
        })
    }

    pub fn owner(&self) -> TerminalHistoryOwner {
        TerminalHistoryOwner::EphemeralAgent {
            terminal_id: self.terminal_id.clone(),
        }
    }

    pub fn push(&mut self, command: &str) -> Result<()> {
        validate_command(command)?;
        if self.commands.back().is_some_and(|last| last == command) {
            return Ok(());
        }
        self.commands.push_back(command.to_owned());
        while self.commands.len() > self.max_entries {
            self.commands.pop_front();
        }
        Ok(())
    }

    pub fn seed(&self) -> Result<TerminalHistorySeed> {
        TerminalHistorySeed::from_commands(self.commands.iter().cloned().collect())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryLine {
    command: String,
}

fn workspace_digest(workspace_root: &Path) -> Result<String> {
    let canonical = workspace_root.canonicalize().map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("history workspace cannot be canonicalized: {error}"),
        )
    })?;
    if canonical != workspace_root || !canonical.is_dir() {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "history workspace must be an existing canonical directory",
        ));
    }
    let mut digest = Sha256::new();
    digest.update(b"agentlibre.human-shell-history.v1\0");
    digest.update(canonical.as_os_str().as_bytes());
    let bytes = digest.finalize();
    let mut rendered = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut rendered, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(rendered)
}

fn validate_command(command: &str) -> Result<()> {
    if command.is_empty() || command.len() > MAX_HISTORY_COMMAND_BYTES || command.contains('\0') {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "shell history command must be nonempty, bounded, and contain no NUL",
        ));
    }
    Ok(())
}

fn trim_history(mut commands: Vec<String>, max_entries: usize, max_bytes: usize) -> Vec<String> {
    let mut total = commands
        .iter()
        .map(|command| encoded_line_size(command))
        .sum::<usize>();
    let mut remove = 0;
    while commands.len().saturating_sub(remove) > max_entries || total > max_bytes {
        total = total.saturating_sub(encoded_line_size(&commands[remove]));
        remove += 1;
    }
    commands.drain(..remove);
    commands
}

fn encoded_line_size(command: &str) -> usize {
    serde_json::to_vec(&HistoryLine {
        command: command.to_owned(),
    })
    .map_or(0, |line| line.len().saturating_add(1))
}

fn read_history(path: &Path, max_entries: usize, max_bytes: usize) -> Result<Vec<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(history_io("failed to open shell history", error)),
    };
    validate_private_file(&file, "shell history")?;
    let file_bytes = file
        .metadata()
        .map_err(|error| history_io("failed to inspect shell history", error))?
        .len();
    if file_bytes > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "shell history file exceeds its private storage bound",
        ));
    }
    let read_limit = max_bytes.saturating_add(1);
    let mut bytes = Vec::with_capacity(
        usize::try_from(file_bytes)
            .unwrap_or(max_bytes)
            .min(max_bytes),
    );
    file.take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| history_io("failed to read shell history", error))?;
    if bytes.len() > max_bytes {
        return Err(ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "shell history file grew beyond its private storage bound",
        ));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::StoreCorrupt,
            "shell history contains invalid UTF-8",
        )
    })?;
    let mut commands = Vec::new();
    for line in text.lines() {
        if line.len() > MAX_HISTORY_COMMAND_BYTES.saturating_mul(2) {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "shell history line exceeds its private storage bound",
            ));
        }
        if commands.len() == max_entries {
            return Err(ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "shell history entry count exceeds its private storage bound",
            ));
        }
        let entry = serde_json::from_str::<HistoryLine>(line).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                format!("shell history contains invalid JSONL: {error}"),
            )
        })?;
        validate_command(&entry.command).map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::StoreCorrupt,
                "shell history contains an invalid command",
            )
        })?;
        commands.push(entry.command);
    }
    Ok(commands)
}

fn write_history_atomic(directory: &Path, path: &Path, commands: &[String]) -> Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".history.{}.{}.tmp", std::process::id(), sequence));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(|error| history_io("failed to create compacted shell history", error))?;
    validate_private_file(&file, "temporary shell history")?;
    let write_result = (|| {
        let mut writer = BufWriter::new(&file);
        for command in commands {
            serde_json::to_writer(
                &mut writer,
                &HistoryLine {
                    command: command.clone(),
                },
            )
            .map_err(|error| {
                ProcessError::new(
                    ProcessErrorCode::Internal,
                    format!("failed to encode shell history: {error}"),
                )
            })?;
            writer
                .write_all(b"\n")
                .map_err(|error| history_io("failed to write shell history", error))?;
        }
        writer
            .flush()
            .map_err(|error| history_io("failed to flush shell history", error))?;
        file.sync_all()
            .map_err(|error| history_io("failed to sync shell history", error))?;
        fs::rename(&temporary, path)
            .map_err(|error| history_io("failed to publish shell history", error))?;
        let directory_file = File::open(directory)
            .map_err(|error| history_io("failed to open shell history directory", error))?;
        directory_file
            .sync_all()
            .map_err(|error| history_io("failed to sync shell history directory", error))
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn lock_history(directory: &Path) -> Result<File> {
    let path = directory.join("history.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| history_io("failed to open shell history lock", error))?;
    validate_private_file(&file, "shell history lock")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(history_io(
            "failed to lock shell history",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(file)
}

fn unlock_history(file: File) -> Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    drop(file);
    if result != 0 {
        return Err(history_io(
            "failed to unlock shell history",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.as_os_str().to_string_lossy().contains('\0') {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "shell history root must be an absolute path",
        ));
    }
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| history_io("failed to protect shell history directory", error))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "shell history directory has no parent",
                )
            })?;
            ensure_private_directory(parent)?;
            fs::create_dir(path)
                .map_err(|error| history_io("failed to create shell history directory", error))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|error| history_io("failed to protect shell history directory", error))?;
        }
        Err(error) => {
            return Err(history_io(
                "failed to create shell history directory",
                error,
            ));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| history_io("failed to inspect shell history directory", error))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "shell history directory must be owned by this user, private, and not a symlink",
        ));
    }
    Ok(())
}

fn validate_private_file(file: &File, label: &str) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| history_io(&format!("failed to inspect {label}"), error))?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            format!("{label} must be an owned private single-link regular file"),
        ));
    }
    Ok(())
}

fn history_io(context: &str, error: std::io::Error) -> ProcessError {
    ProcessError::new(ProcessErrorCode::Internal, format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "agl-terminal-history-{name}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let history = base.join("private/history");
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        (history, workspace.canonicalize().unwrap())
    }

    #[test]
    fn human_history_is_private_bounded_atomic_and_duplicate_suppressed() {
        let (root, workspace) = fixture("human");
        let store = HumanShellHistoryStore::new(root.clone(), 2, 64 * 1024).unwrap();
        store.append(&workspace, "echo one").unwrap();
        store.append(&workspace, "echo one").unwrap();
        store.append(&workspace, "printf 'two\\nlines'").unwrap();
        store.append(&workspace, "echo three").unwrap();

        let seed = store.load(&workspace).unwrap();
        assert_eq!(
            seed.commands(),
            &["printf 'two\\nlines'".to_owned(), "echo three".to_owned()]
        );
        let directory = root.join(workspace_digest(&workspace).unwrap());
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("history.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(workspace.parent().unwrap()).unwrap();
    }

    #[test]
    fn agent_history_never_touches_disk_and_dies_with_owner() {
        let terminal_id = TerminalId::generate();
        let mut history = EphemeralTerminalHistory::new(terminal_id.clone(), 2).unwrap();
        history.push("cd one").unwrap();
        history.push("cd two").unwrap();
        history.push("pwd").unwrap();
        assert_eq!(history.seed().unwrap().commands(), &["cd two", "pwd"]);
        assert_eq!(
            history.owner(),
            TerminalHistoryOwner::EphemeralAgent { terminal_id }
        );
    }

    #[test]
    fn human_history_rejects_oversized_files_and_entry_counts_before_growth() {
        let (root, workspace) = fixture("bounded-read");
        let store = HumanShellHistoryStore::new(root, 2, MAX_HISTORY_COMMAND_BYTES).unwrap();
        let directory = store.workspace_directory(&workspace).unwrap();
        let path = directory.join("history.jsonl");

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(&vec![b'x'; MAX_HISTORY_COMMAND_BYTES + 1])
            .unwrap();
        drop(file);
        assert_eq!(
            store.load(&workspace).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );

        fs::remove_file(&path).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        for command in ["one", "two", "three"] {
            serde_json::to_writer(
                &mut file,
                &HistoryLine {
                    command: command.to_owned(),
                },
            )
            .unwrap();
            file.write_all(b"\n").unwrap();
        }
        drop(file);
        assert_eq!(
            store.load(&workspace).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );
        fs::remove_dir_all(workspace.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn human_history_rejects_symlink_non_regular_and_corrupt_records() {
        use std::os::unix::fs::symlink;

        let (root, workspace) = fixture("invalid-files");
        let store = HumanShellHistoryStore::with_defaults(root).unwrap();
        let directory = store.workspace_directory(&workspace).unwrap();
        let path = directory.join("history.jsonl");
        let target = directory.join("outside.jsonl");
        let target_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)
            .unwrap();
        drop(target_file);
        symlink(&target, &path).unwrap();
        assert!(store.load(&workspace).is_err());

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(store.load(&workspace).is_err());

        fs::remove_dir(&path).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"not-json\n").unwrap();
        drop(file);
        assert_eq!(
            store.load(&workspace).unwrap_err().code(),
            ProcessErrorCode::StoreCorrupt
        );
        fs::remove_dir_all(workspace.parent().unwrap()).unwrap();
    }

    #[test]
    fn workspace_history_keys_hash_exact_linux_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let base = std::env::temp_dir().join(format!(
            "agl-terminal-history-non-utf8-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&base).unwrap();
        let first = base.join(OsString::from_vec(vec![b'w', 0x80]));
        let second = base.join(OsString::from_vec(vec![b'w', 0x81]));
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();

        assert_ne!(
            workspace_digest(&first).unwrap(),
            workspace_digest(&second).unwrap()
        );
        let store = HumanShellHistoryStore::with_defaults(base.join("history")).unwrap();
        store.append(&first, "echo first").unwrap();
        store.append(&second, "echo second").unwrap();
        assert_eq!(store.load(&first).unwrap().commands(), ["echo first"]);
        assert_eq!(store.load(&second).unwrap().commands(), ["echo second"]);

        fs::remove_dir_all(base).unwrap();
    }
}
