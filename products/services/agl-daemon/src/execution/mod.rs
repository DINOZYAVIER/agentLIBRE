//! Durable AGL bindings and bounded output. systemd owns processes; tmux owns terminals.
mod fsm;
mod helper;
mod planner_cache;
mod private;
mod readonly;
mod sandbox;

use crate::store::StoreHandle;
use agl_core::agent::{AgentOperationKey, ToolFailure, ToolFailureKind};
use agl_core::execution::*;
use agl_runtime::inference::{PrivateInference, ProcessAction, ProcessHost, ProcessResult};
use agl_tool_api::{ExecutionHost, ToolContext};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) use fsm::{Completion, ExecutionFsm, ExecutionInput, MainOutcome, Streams};
pub use helper::run_helper;
pub use planner_cache::clear as clear_planner_cache;

#[derive(Clone)]
pub struct ExecutionBackend(Arc<Backend>);
struct Backend {
    root: PathBuf,
    socket: PathBuf,
    helper: PathBuf,
    owner_unit: String,
    tmux_unit: String,
    store: StoreHandle,
    launch_lock: Mutex<()>,
    systemd_run: PathBuf,
    systemctl: PathBuf,
    tmux: PathBuf,
}
#[derive(Serialize, Deserialize)]
struct Launch {
    request: ExecutionStartRequest,
    private: Option<PrivateInference>,
    helper: PathBuf,
    systemd_run: PathBuf,
    systemctl: PathBuf,
    tmux: PathBuf,
    socket: PathBuf,
    unit: String,
    owner_unit: String,
    tmux_unit: String,
    memory_max: Option<u64>,
    input_file: Option<PathBuf>,
    input_fd_path: Option<PathBuf>,
    output_fd_path: Option<PathBuf>,
    deadline_at_ms: Option<i64>,
    bubblewrap: Option<PathBuf>,
    planner_cache: Option<planner_cache::PlannerCache>,
}
struct GitFiles {
    stdin: Option<File>,
    stdout: Option<File>,
}
enum LaunchInput<'a> {
    Bytes(&'a [u8]),
    Files(&'a GitFiles),
}
fn process_fd_path(file: &File) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        file.as_raw_fd()
    ))
}
impl ExecutionBackend {
    pub(crate) fn new(
        data: &Path,
        state: &Path,
        store: StoreHandle,
        owner_unit: String,
    ) -> Result<Self> {
        let root = data.join("executions");
        private_dir(&root)?;
        let socket = state.join("executions/tmux.sock");
        private_dir(socket.parent().unwrap())?;
        let digest = Sha256::digest(socket.as_os_str().as_encoded_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = format!("agentlibre-tmux-{digest}.service");
        let backend = Self(Arc::new(Backend {
            root,
            socket,
            helper: std::env::current_exe()?,
            owner_unit,
            tmux_unit: name,
            store,
            launch_lock: Mutex::new(()),
            systemd_run: program("systemd-run")?,
            systemctl: program("systemctl")?,
            tmux: program("tmux")?,
        }));
        readonly::Scratch::recover(&backend.0.root, |id| Ok(!backend.unit_live(&unit(id))?))?;
        Ok(backend)
    }
    pub fn process_host(&self) -> ProcessHost {
        let backend = self.clone();
        ProcessHost::new(move |action| {
            (|| -> Result<ProcessResult> {
                Ok(match action {
                    ProcessAction::Start { request, isolation } => ProcessResult::Status(
                        backend.launch(None, *request, Some(isolation), None)?,
                    ),
                    ProcessAction::Inspect(id) => ProcessResult::Status(backend.inspect(id)?),
                    ProcessAction::Read {
                        execution_id,
                        offset,
                        max_bytes,
                    } => ProcessResult::Output(backend.read(
                        execution_id,
                        ExecutionOutputStream::Stderr,
                        offset,
                        max_bytes,
                    )?),
                    ProcessAction::Stop(id) => {
                        backend.stop(id, ExecutionStopCause::Stop)?;
                        ProcessResult::Stopped
                    }
                })
            })()
            .map_err(|error| error.to_string())
        })
    }
    pub fn tool_host(&self) -> ExecutionHost {
        let backend = self.clone();
        ExecutionHost::new(move |context, action| {
            let backend = backend.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || backend.tool_action(context, action))
                    .await
                    .map_err(|_| ToolFailure::unknown(ToolFailureKind::OutcomeUnknown))?
            })
        })
    }
    fn tool_action(
        &self,
        context: ToolContext,
        action: ExecutionAction,
    ) -> std::result::Result<ExecutionResult, ToolFailure> {
        if context.is_cancelled() {
            return Err(ToolFailure::no_effect(
                ToolFailureKind::Cancelled,
                None,
                &[],
            ));
        }
        if now_ms() >= context.deadline_at_ms {
            return Err(ToolFailure::no_effect(ToolFailureKind::Deadline, None, &[]));
        }
        let id = match &action {
            ExecutionAction::Start(_) => None,
            ExecutionAction::Inspect(id)
            | ExecutionAction::Interrupt(id)
            | ExecutionAction::Stop(id)
            | ExecutionAction::Cleanup(id) => Some(*id),
            ExecutionAction::Read { execution_id, .. }
            | ExecutionAction::Wait { execution_id, .. }
            | ExecutionAction::Write { execution_id, .. }
            | ExecutionAction::Resize { execution_id, .. } => Some(*execution_id),
        };
        if let Some(id) = id {
            let (request, _, _) = self
                .0
                .store
                .execution(id)
                .map_err(|_| unavailable_execution_id())?;
            if !matches!(request.owner,ExecutionOwner::Agent {conversation_id,..} if Some(conversation_id)==context.conversation_id)
            {
                return Err(ToolFailure::no_effect(
                    ToolFailureKind::Unauthorized,
                    Some("execution_id"),
                    &["Use an execution_id created by a successful start in this Conversation."],
                ));
            }
        }
        let result = (|| -> Result<ExecutionResult> {
            Ok(match action {
                ExecutionAction::Start(mut request) => {
                    request.owner = ExecutionOwner::Agent {
                        conversation_id: context
                            .conversation_id
                            .context("Conversation required")?,
                        run_id: context.operation.run_id,
                    };
                    ensure!(
                        context.read_only_workspace
                            == matches!(
                                request.isolation,
                                ExecutionIsolation::ReadOnlyWorkspace { .. }
                            ),
                        "incorrect execution profile"
                    );
                    let deadline = context
                        .read_only_workspace
                        .then_some(context.deadline_at_ms);
                    let status = self.launch_with(
                        ExecutionId::generate(),
                        Some(context.operation.clone()),
                        request,
                        None,
                        None,
                        None,
                        deadline,
                        Some(&context.cancellation()),
                    )?;
                    if context.is_cancelled() {
                        self.stop(status.execution_id, ExecutionStopCause::RunCancel)?;
                    }
                    ExecutionResult::Status(status)
                }
                ExecutionAction::Inspect(id) => ExecutionResult::Status(self.inspect(id)?),
                ExecutionAction::Read {
                    execution_id,
                    stream,
                    offset,
                    max_bytes,
                } => ExecutionResult::Output(self.read(execution_id, stream, offset, max_bytes)?),
                ExecutionAction::Wait {
                    execution_id,
                    wait_timeout_ms,
                } => {
                    let until = Instant::now()
                        + Duration::from_millis(
                            wait_timeout_ms
                                .min(context.deadline_at_ms.saturating_sub(now_ms()) as u64),
                        );
                    let status = loop {
                        let status = self.inspect(execution_id)?;
                        if status.state != ExecutionState::Running
                            || Instant::now() >= until
                            || context.is_cancelled()
                        {
                            break status;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    };
                    ExecutionResult::Status(status)
                }
                ExecutionAction::Write { execution_id, data } => {
                    ensure!(
                        !data.is_empty() && data.len() <= 65536,
                        "terminal write bound"
                    );
                    self.terminal_write(execution_id, &data)?;
                    ExecutionResult::Acknowledged
                }
                ExecutionAction::Interrupt(id) => {
                    self.terminal_write(id, &[3])?;
                    ExecutionResult::Acknowledged
                }
                ExecutionAction::Resize { execution_id, size } => {
                    ensure!(
                        (1..=1000).contains(&size.rows) && (1..=1000).contains(&size.columns),
                        "terminal size bound"
                    );
                    self.terminal_running(execution_id)?;
                    self.tmux(&[
                        "resize-window",
                        "-t",
                        &session(execution_id),
                        "-x",
                        &size.columns.to_string(),
                        "-y",
                        &size.rows.to_string(),
                    ])?;
                    ExecutionResult::Acknowledged
                }
                ExecutionAction::Stop(id) => {
                    self.stop(id, ExecutionStopCause::Stop)?;
                    ExecutionResult::Acknowledged
                }
                ExecutionAction::Cleanup(id) => {
                    self.cleanup(id)?;
                    ExecutionResult::Acknowledged
                }
            })
        })();
        result.map_err(|error| {
            tracing::warn!(%error,"execution action failed");
            ToolFailure::unknown(ToolFailureKind::OutcomeUnknown)
        })
    }
    fn ensure_tmux(&self) -> Result<()> {
        if self.unit_live(&self.0.tmux_unit)? {
            return self.install_session_hook();
        }
        let mut command = Command::new(&self.0.systemd_run);
        command
            .args([
                "--user",
                "--quiet",
                "--service-type=exec",
                "--expand-environment=no",
                "--collect",
            ])
            .arg(format!("--unit={}", self.0.tmux_unit))
            .args([
                "--property=Restart=no",
                "--property=KillMode=control-group",
                "--property=UMask=0077",
            ])
            .arg("--")
            .arg(&self.0.tmux)
            .args(["-D", "-f", "/dev/null", "-S"])
            .arg(&self.0.socket);
        checked(&mut command)?;
        let ready = Instant::now() + Duration::from_secs(5);
        while !self.0.socket.exists() {
            ensure!(
                Instant::now() < ready,
                "tmux server did not publish its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        self.tmux(&[
            "new-session",
            "-d",
            "-s",
            "agl-keeper",
            &program("sleep")?.to_string_lossy(),
            "infinity",
        ])?;
        self.tmux(&["set-option", "-g", "remain-on-exit", "off"])?;
        self.install_session_hook()
    }
    fn install_session_hook(&self) -> Result<()> {
        let callback = format!(
            "{} __execution session-closed {}/#{{q:hook_session_name}}",
            shell_quote(&self.0.helper.to_string_lossy()),
            shell_quote(&self.0.root.to_string_lossy()),
        );
        self.tmux(&[
            "set-hook",
            "-g",
            "session-closed",
            &format!("run-shell -b {}", shell_quote(&callback)),
        ])
    }
    pub(crate) fn launch(
        &self,
        operation: Option<AgentOperationKey>,
        request: ExecutionStartRequest,
        private: Option<PrivateInference>,
        memory_max: Option<u64>,
    ) -> Result<ExecutionStatus> {
        self.launch_with(
            ExecutionId::generate(),
            operation,
            request,
            private,
            memory_max,
            None,
            None,
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn launch_with(
        &self,
        id: ExecutionId,
        operation: Option<AgentOperationKey>,
        request: ExecutionStartRequest,
        private: Option<PrivateInference>,
        memory_max: Option<u64>,
        input: Option<LaunchInput<'_>>,
        deadline_at_ms: Option<i64>,
        cancellation: Option<&agl_tool_api::ToolCancellation>,
    ) -> Result<ExecutionStatus> {
        request.validate()?;
        if let Some(private) = &private {
            private.validate(&request).map_err(anyhow::Error::msg)?;
        }
        let _guard = self
            .0
            .launch_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("launch lock poisoned"))?;
        ensure!(
            deadline_at_ms.is_none_or(|deadline| now_ms() < deadline),
            "execution deadline expired before reservation"
        );
        ensure!(
            cancellation.is_none_or(|cancellation| !cancellation.is_cancelled()),
            "execution cancelled before reservation"
        );
        let initial = ExecutionFsm::initial(id, request.io);
        let (id, new) = self
            .0
            .store
            .reserve_execution(operation, &request, &initial)?;
        if !new {
            return self.inspect_unlocked(id);
        }
        let directory = self.directory(id);
        private_dir(&directory)?;
        let mut attempt = LaunchAttempt {
            backend: self.clone(),
            id,
            confirmed: false,
        };
        self.ensure_tmux()?;
        let protected = private.is_some()
            || !matches!(request.isolation, ExecutionIsolation::Standard)
            || matches!(request.owner, ExecutionOwner::Runtime { .. });
        ensure!(
            !protected || !self.0.owner_unit.is_empty(),
            "protected execution requires managed daemon"
        );
        let (input_fd_path, output_fd_path) = match &input {
            Some(LaunchInput::Files(files)) => (
                files.stdin.as_ref().map(process_fd_path),
                files.stdout.as_ref().map(process_fd_path),
            ),
            _ => (None, None),
        };
        let input_file = if let Some(LaunchInput::Bytes(input)) = input {
            let file = directory.join("input");
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)?;
            output.write_all(input)?;
            output.sync_all()?;
            Some(file)
        } else {
            None
        };
        let bubblewrap = if matches!(
            request.isolation,
            ExecutionIsolation::ReadOnlyWorkspace { .. }
        ) {
            Some(program("bwrap")?)
        } else {
            None
        };
        let planner_cache = match &request.isolation {
            ExecutionIsolation::ReadOnlyWorkspace { workspace_root, .. } => {
                Some(planner_cache::PlannerCache::prepare(
                    self.0.root.parent().unwrap(),
                    Path::new(workspace_root),
                )?)
            }
            _ => None,
        };
        let launch = Launch {
            planner_cache,
            bubblewrap,
            deadline_at_ms,
            input_file,
            input_fd_path,
            output_fd_path,
            request: request.clone(),
            private,
            helper: self.0.helper.clone(),
            systemd_run: self.0.systemd_run.clone(),
            systemctl: self.0.systemctl.clone(),
            tmux: self.0.tmux.clone(),
            socket: self.0.socket.clone(),
            unit: unit(id),
            tmux_unit: self.0.tmux_unit.clone(),
            owner_unit: if protected {
                self.0.owner_unit.clone()
            } else {
                self.0.tmux_unit.clone()
            },
            memory_max,
        };
        atomic_json(&directory.join("launch.json"), &launch)?;

        ensure!(
            cancellation.is_none_or(|cancellation| !cancellation.is_cancelled()),
            "execution cancelled before unit launch; reservation retained"
        );
        ensure!(
            deadline_at_ms.is_none_or(|deadline| now_ms() < deadline),
            "execution deadline expired before unit launch; reservation retained"
        );
        ensure!(
            read_json::<ExecutionStopCause>(&directory.join("stop.json")).ok()
                != Some(ExecutionStopCause::TmuxLost),
            "tmux session was lost before unit launch; reservation retained"
        );

        match request.io {
            ExecutionIo::Pipes => {
                fifo(&directory.join("stdout.fifo"))?;
                fifo(&directory.join("stderr.fifo"))?;
                self.tmux_args(&[
                    "new-session".into(),
                    "-d".into(),
                    "-s".into(),
                    session(id),
                    self.0.helper.to_string_lossy().into_owned(),
                    "__execution".into(),
                    "capture-command".into(),
                    directory.to_string_lossy().into_owned(),
                ])?;
                let mut command = helper::run_command(&launch, &directory, None);
                checked(&mut command)?;
            }
            ExecutionIo::Pty => {
                let size = request.terminal_size.unwrap_or_default();
                self.tmux_args(&[
                    "new-session".into(),
                    "-d".into(),
                    "-s".into(),
                    session(id),
                    "-x".into(),
                    size.columns.to_string(),
                    "-y".into(),
                    size.rows.to_string(),
                    self.0.helper.to_string_lossy().into_owned(),
                    "__execution".into(),
                    "terminal".into(),
                    directory.to_string_lossy().into_owned(),
                ])?;
                let pipe = format!(
                    "{} __execution capture-terminal {}",
                    shell_quote(&self.0.helper.to_string_lossy()),
                    shell_quote(&directory.to_string_lossy())
                );
                self.tmux(&["pipe-pane", "-O", "-t", &session(id), &pipe])?;
            }
        }
        let ready = Instant::now() + Duration::from_secs(10);
        while !directory.join("started.json").exists() && !directory.join("outcome.json").exists() {
            let stop = if cancellation.is_some_and(|cancellation| cancellation.is_cancelled()) {
                Some(ExecutionStopCause::RunCancel)
            } else if deadline_at_ms.is_some_and(|deadline| now_ms() >= deadline) {
                Some(ExecutionStopCause::Timeout)
            } else {
                None
            };
            if let Some(cause) = stop {
                self.stop_unlocked(id, cause)?;
                attempt.confirmed = true;
                return self.inspect_unlocked(id);
            }
            ensure!(
                Instant::now() < ready,
                "execution startup was not confirmed; reservation retained"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        attempt.confirmed = true;
        self.inspect_unlocked(id)
    }
    pub(crate) fn inspect(&self, id: ExecutionId) -> Result<ExecutionStatus> {
        let _guard = self
            .0
            .launch_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("launch lock poisoned"))?;
        self.inspect_unlocked(id)
    }
    fn inspect_unlocked(&self, id: ExecutionId) -> Result<ExecutionStatus> {
        let (_, status, cause) = self.0.store.execution(id)?;
        if status.outputs_removed || status.state != ExecutionState::Running {
            return Ok(status);
        }
        let directory = self.directory(id);
        let mut completion = read_json::<ExecutionStatus>(&directory.join("result.json")).ok();
        let unit_live = completion.is_some() || self.unit_live(&unit(id))?;
        if completion.is_none() && !unit_live && directory.join("outcome.json").exists() {
            let until = Instant::now() + Duration::from_secs(2);
            while !directory.join("result.json").exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            completion = read_json(&directory.join("result.json")).ok();
        }
        let mut streams = Streams::from(completion.as_ref().unwrap_or(&status));
        for (stream, target) in [
            ("stdout", &mut streams.stdout),
            ("stderr", &mut streams.stderr),
            ("terminal", &mut streams.terminal),
        ] {
            if let Some(value) = target {
                if let Ok(saved) = read_json(&directory.join(format!("{stream}.json"))) {
                    *value = saved;
                } else {
                    if let Ok(saved) = read_json(&directory.join(format!("{stream}.live.json"))) {
                        *value = saved;
                    }
                    if let Ok(metadata) = fs::metadata(directory.join(stream)) {
                        value.bytes = metadata.len();
                    }
                }
            }
        }
        Ok(self.0.store.apply_execution_input(
            id,
            ExecutionInput::Observe {
                unit_live,
                completion,
                streams,
                stop_cause: cause,
            },
        )?)
    }
    pub(crate) fn read(
        &self,
        id: ExecutionId,
        stream: ExecutionOutputStream,
        offset: u64,
        max_bytes: u32,
    ) -> Result<ExecutionOutput> {
        ensure!(
            (1..=MAX_EXECUTION_OUTPUT_READ_BYTES).contains(&max_bytes),
            "read bound"
        );
        let status = self.inspect(id)?;
        ensure!(!status.outputs_removed, "outputs removed");
        let (name, meta) = match stream {
            ExecutionOutputStream::Stdout => ("stdout", status.stdout),
            ExecutionOutputStream::Stderr => ("stderr", status.stderr),
            ExecutionOutputStream::Pty => ("terminal", status.terminal),
        };
        let meta = meta.context("stream unavailable for this execution")?;
        ensure!(offset <= meta.bytes, "offset beyond output");
        let path = self.directory(id).join(name);
        let mut data = Vec::new();
        if path.exists() {
            let mut file = open_regular(&path)?;
            file.seek(SeekFrom::Start(offset))?;
            file.take(u64::from(max_bytes)).read_to_end(&mut data)?;
        }
        let next = offset + data.len() as u64;
        Ok(ExecutionOutput {
            execution_id: id,
            stream,
            offset,
            next,
            data,
            eof: status.state != ExecutionState::Running && next >= meta.bytes,
            truncated: meta.truncated,
        })
    }
    fn terminal_running(&self, id: ExecutionId) -> Result<()> {
        let status = self.inspect(id)?;
        ensure!(
            status.terminal.is_some() && status.state == ExecutionState::Running,
            "terminal is not running"
        );
        Ok(())
    }
    fn terminal_write(&self, id: ExecutionId, data: &[u8]) -> Result<()> {
        self.terminal_running(id)?;
        let buffer = format!("agl-{}", uuid::Uuid::now_v7());
        let mut command = self.tmux_command();
        command
            .args(["load-buffer", "-b", &buffer, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        child.stdin.take().context("tmux input")?.write_all(data)?;
        ensure!(child.wait()?.success(), "tmux load-buffer failed");
        let result = self.tmux(&[
            "paste-buffer",
            "-rS",
            "-d",
            "-b",
            &buffer,
            "-t",
            &session(id),
        ]);
        if result.is_err() {
            let _ = self.tmux(&["delete-buffer", "-b", &buffer]);
        }
        result
    }
    pub(crate) fn stop(&self, id: ExecutionId, cause: ExecutionStopCause) -> Result<()> {
        let _guard = self
            .0
            .launch_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("launch lock poisoned"))?;
        self.stop_unlocked(id, cause)
    }
    fn stop_unlocked(&self, id: ExecutionId, cause: ExecutionStopCause) -> Result<()> {
        if self.inspect_unlocked(id)?.state != ExecutionState::Running {
            return Ok(());
        }
        self.0.store.execution_stop_cause(id, cause)?;
        atomic_json(&self.directory(id).join("stop.json"), &cause)?;
        checked(Command::new(&self.0.systemctl).args(["--user", "stop", &unit(id)]))?;
        self.inspect_unlocked(id)?;
        Ok(())
    }
    pub(crate) fn cancel_run(&self, run_id: agl_core::AgentRunId) -> Result<()> {
        let _guard = self
            .0
            .launch_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("launch lock poisoned"))?;
        for id in self.0.store.cancel_execution_run(run_id)? {
            self.stop_unlocked(id, ExecutionStopCause::RunCancel)?;
        }
        Ok(())
    }
    pub(crate) fn cleanup(&self, id: ExecutionId) -> Result<()> {
        let _guard = self
            .0
            .launch_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("launch lock poisoned"))?;
        let status = self.inspect_unlocked(id)?;
        ensure!(
            status.state != ExecutionState::Running,
            "cannot clean running execution"
        );
        if !status.outputs_removed {
            let directory = self.directory(id);
            if directory.exists() {
                fs::remove_dir_all(directory)?;
            }
            let _ = self.tmux(&["kill-session", "-t", &session(id)]);
            self.0
                .store
                .apply_execution_input(id, ExecutionInput::OutputsRemoved)?;
        }
        Ok(())
    }
    fn unit_live(&self, name: &str) -> Result<bool> {
        let output = Command::new(&self.0.systemctl)
            .args(["--user", "show", "--property=ActiveState", "--value", name])
            .output()?;
        ensure!(
            output.status.success(),
            "cannot query user systemd: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(matches!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "active" | "activating" | "deactivating"
        ))
    }
    fn directory(&self, id: ExecutionId) -> PathBuf {
        self.0.root.join(id.to_string())
    }
    fn tmux_command(&self) -> Command {
        let mut command = Command::new(&self.0.tmux);
        command.args(["-S"]).arg(&self.0.socket);
        command
    }
    fn tmux(&self, args: &[&str]) -> Result<()> {
        checked(self.tmux_command().args(args)).map(|_| ())
    }
    fn tmux_args(&self, args: &[String]) -> Result<()> {
        checked(self.tmux_command().args(args)).map(|_| ())
    }
}

fn unavailable_execution_id() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::Unavailable,
        Some("execution_id"),
        &[
            "Use an execution_id returned by a successful start in this Conversation; if it is valid, retry when the execution service is available.",
        ],
    )
}
fn unit(id: ExecutionId) -> String {
    format!("agentlibre-{id}.service")
}
fn session(id: ExecutionId) -> String {
    id.to_string()
}
fn program(name: &str) -> Result<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .filter(|p| p.is_absolute())
        .map(|p| p.join(name))
        .find(|p| p.is_file())
        .with_context(|| format!("required tool {name} is absent from PATH"))
}
fn checked(command: &mut Command) -> Result<std::process::Output> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "private directory is a symlink"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn fifo(path: &Path) -> Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    ensure!(
        unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == 0,
        "cannot create FIFO: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "not a regular file");
    Ok(file)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_reader(open_regular(path)?)?)
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceGitConfig {
    pub memory_max_bytes: u64,
    pub max_output_bytes: u64,
}
impl ServiceGitConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.memory_max_bytes > 0,
            "service_git.memory_max_bytes must be positive"
        );
        ensure!(
            self.max_output_bytes > 0,
            "service_git.max_output_bytes must be positive"
        );
        Ok(())
    }
}
impl Default for ServiceGitConfig {
    fn default() -> Self {
        Self {
            memory_max_bytes: 1024 * 1024 * 1024,
            max_output_bytes: 1024 * 1024,
        }
    }
}
impl ExecutionBackend {
    pub(crate) fn service_git(&self, limits: ServiceGitConfig) -> agl_tool_api::ServiceGit {
        let backend = self.clone();
        Arc::new(move |request| backend.run_git(request, limits))
    }
    pub(crate) fn run_forge_git(
        &self,
        request: ayeque_forge_core::GitCommand,
        limits: ServiceGitConfig,
    ) -> Result<std::process::Output> {
        use std::os::unix::process::ExitStatusExt;
        let remaining = request.deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Forge sync deadline exceeded");
        let files = GitFiles {
            stdin: request.stdin,
            stdout: request.stdout,
        };
        let output = self
            .run_git_with_files(
                agl_tool_api::ServiceGitRequest {
                    cwd: request.cwd,
                    args: request.args,
                    env: ["PATH", "HOME", "XDG_CONFIG_HOME", "SSH_AUTH_SOCK", "LC_ALL"]
                        .into_iter()
                        .filter_map(|name| std::env::var_os(name).map(|value| (name.into(), value)))
                        .collect(),
                    stdin: Vec::new(),
                    timeout: remaining,
                    deadline_at_ms: now_ms()
                        .saturating_add(remaining.as_millis().min(i64::MAX as u128) as i64),
                    cancellation: agl_tool_api::ToolCancellation::new(),
                },
                limits,
                Some(&files),
            )
            .map_err(|_| anyhow::anyhow!("Forge Git was not started"))?;
        ensure!(!output.timed_out, "Forge sync deadline exceeded");
        ensure!(!output.cancelled, "Forge Git was cancelled");
        ensure!(
            !output.stdout_truncated || files.stdout.is_some(),
            "Forge Git metadata output exceeded service_git.max_output_bytes"
        );
        let code = output
            .exit_code
            .context("Forge Git completion is unconfirmed")?;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
    fn run_git(
        &self,
        request: agl_tool_api::ServiceGitRequest,
        limits: ServiceGitConfig,
    ) -> std::result::Result<agl_tool_api::ServiceGitOutput, agl_tool_api::ServiceGitNotStarted>
    {
        self.run_git_with_files(request, limits, None)
    }
    fn run_git_with_files(
        &self,
        request: agl_tool_api::ServiceGitRequest,
        limits: ServiceGitConfig,
        files: Option<&GitFiles>,
    ) -> std::result::Result<agl_tool_api::ServiceGitOutput, agl_tool_api::ServiceGitNotStarted>
    {
        use agl_tool_api::{ServiceGitNotStarted, ServiceGitOutput};
        if request.cancellation.is_cancelled()
            || now_ms() >= request.deadline_at_ms
            || limits.memory_max_bytes == 0
            || limits.max_output_bytes == 0
        {
            return Err(ServiceGitNotStarted);
        }
        let git = program("git").map_err(|_| ServiceGitNotStarted)?;
        let convert = |value: &std::ffi::OsStr| {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or(ServiceGitNotStarted)
        };
        let mut argv = vec![git.to_string_lossy().into_owned()];
        for arg in &request.args {
            argv.push(convert(arg)?);
        }
        let mut environment = std::collections::BTreeMap::new();
        for (key, value) in &request.env {
            environment.insert(convert(key)?, convert(value)?);
        }
        let start = ExecutionStartRequest {
            owner: ExecutionOwner::Runtime {
                component: "service-git".into(),
            },
            argv,
            cwd: request.cwd.to_str().ok_or(ServiceGitNotStarted)?.to_owned(),
            environment,
            clear_environment: true,
            io: ExecutionIo::Pipes,
            timeout_ms: request
                .deadline_at_ms
                .saturating_sub(now_ms())
                .min(request.timeout.as_millis().min(u64::MAX as u128) as i64)
                .min(3_600_000) as u64,
            max_output_bytes: limits.max_output_bytes,
            terminal_size: None,
            isolation: ExecutionIsolation::Standard,
        };
        start.validate().map_err(|_| ServiceGitNotStarted)?;
        let deadline = request
            .deadline_at_ms
            .min(now_ms().saturating_add(request.timeout.as_millis().min(i64::MAX as u128) as i64));
        if now_ms() >= deadline || request.cancellation.is_cancelled() {
            return Err(ServiceGitNotStarted);
        }
        let id = ExecutionId::generate();
        let mut output = ServiceGitOutput::default();
        // After reservation any transport/startup failure is uncertain. Never
        // claim NotStarted for a lost reply from the manager.
        if let Err(error) = self.launch_with(
            id,
            None,
            start,
            None,
            Some(limits.memory_max_bytes),
            Some(files.map_or(LaunchInput::Bytes(&request.stdin), LaunchInput::Files)),
            Some(deadline),
            Some(&request.cancellation),
        ) {
            tracing::error!(%error,%id,"service Git launch outcome unconfirmed");
            output.cancelled = request.cancellation.is_cancelled();
            output.timed_out = now_ms() >= deadline;
            let _ = checked(Command::new(&self.0.systemctl).args(["--user", "stop", &unit(id)]));
            return Ok(output);
        }
        // Startup can be stopped before payload exec is proven. Its process
        // result remains unknown, but the Tool cancellation/deadline is known.
        output.cancelled = request.cancellation.is_cancelled();
        output.timed_out = now_ms() >= deadline;
        loop {
            let status = match self.inspect(id) {
                Ok(status) => status,
                Err(_) => {
                    let _ = checked(Command::new(&self.0.systemctl).args([
                        "--user",
                        "stop",
                        &unit(id),
                    ]));
                    return Ok(output);
                }
            };
            if status.state != ExecutionState::Running {
                output.exit_code = match status.outcome {
                    Some(ExecutionOutcome::Exit { code }) => Some(code),
                    _ => None,
                };
                output.timed_out |= status.stopped_by == Some(ExecutionStopCause::Timeout);
                output.cancelled |= status.stopped_by == Some(ExecutionStopCause::RunCancel);
                break;
            }
            if now_ms() >= deadline {
                output.timed_out = true;
                if self.stop(id, ExecutionStopCause::Timeout).is_err() {
                    return Ok(output);
                }
            } else if request.cancellation.is_cancelled() {
                output.cancelled = true;
                if self.stop(id, ExecutionStopCause::RunCancel).is_err() {
                    return Ok(output);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let read = |stream| -> Result<(Vec<u8>, bool)> {
            let mut bytes = Vec::new();
            let mut offset = 0;
            loop {
                let result = self.read(id, stream, offset, MAX_EXECUTION_OUTPUT_READ_BYTES)?;
                bytes.extend_from_slice(&result.data);
                offset = result.next;
                if result.eof {
                    return Ok((bytes, result.truncated));
                }
            }
        };
        match (
            read(ExecutionOutputStream::Stdout),
            read(ExecutionOutputStream::Stderr),
        ) {
            (Ok((stdout, stdout_truncated)), Ok((stderr, stderr_truncated))) => {
                output.stdout = stdout;
                output.stdout_truncated = stdout_truncated;
                output.stderr = stderr;
                output.stderr_truncated = stderr_truncated;
            }
            _ => output.exit_code = None,
        }
        let _ = self.cleanup(id);
        Ok(output)
    }
}

#[cfg(test)]
mod tests;

struct LaunchAttempt {
    backend: ExecutionBackend,
    id: ExecutionId,
    confirmed: bool,
}
impl Drop for LaunchAttempt {
    fn drop(&mut self) {
        if !self.confirmed {
            let _ = checked(Command::new(&self.backend.0.systemctl).args([
                "--user",
                "stop",
                &unit(self.id),
            ]));
            let _ = self
                .backend
                .tmux(&["kill-session", "-t", &session(self.id)]);
        }
    }
}
