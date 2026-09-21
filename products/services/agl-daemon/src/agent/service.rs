use super::*;

pub(crate) const PROGRESS_CAPACITY: usize = 128;
pub(crate) const SCHEDULER_QUEUE_CAPACITY: usize = 256;
pub(crate) const MAX_ACTIVE_RUNS: usize = 32;
pub(crate) const RECOVERY_PAGE_CAPACITY: usize = MAX_ACTIVE_RUNS + SCHEDULER_QUEUE_CAPACITY;
#[derive(Clone)]
pub(crate) struct PreparedTool {
    pub(crate) binding: ToolBinding,
    pub(crate) definition: ToolDefinition,
    pub(crate) input_validator: Arc<jsonschema::Validator>,
    pub(crate) extension_id: ExtensionId,
    pub(crate) extension_version: agl_runtime::package::PackageVersion,
    pub(crate) extension_digest: ExtensionDefinitionDigest,
    pub(crate) effect_validators: BTreeMap<EffectId, Arc<jsonschema::Validator>>,
}

#[derive(Clone, Default)]
pub(crate) struct RunCancellation {
    pub(crate) inference: InferenceCancellation,
    pub(crate) tool: ToolCancellation,
}

impl RunCancellation {
    pub(crate) fn cancel(&self) {
        self.inference.cancel();
        self.tool.cancel();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inference.is_cancelled()
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[derive(Clone)]
pub struct AgentDependencies {
    pub store: StoreHandle,
    pub inference: Arc<dyn InferenceRoute>,
}

#[derive(Clone)]
pub struct AgentHandle {
    sender: mpsc::SyncSender<Command>,
    progress: ProgressSubscribers,
}

impl AgentHandle {
    pub(crate) fn start_run(&self, spec: AgentRunSpec) -> Result<AgentRunId, AgentServiceError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(Command::StartRun {
                spec,
                reply: sender,
            })
            .map_err(map_command_send)?;
        receiver.recv().map_err(|_| AgentServiceError::Stopped)?
    }

    pub(crate) fn cancel(&self, run_id: AgentRunId) -> Result<(), AgentServiceError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(Command::CancelRun {
                run_id,
                reply: sender,
            })
            .map_err(map_command_send)?;
        receiver.recv().map_err(|_| AgentServiceError::Stopped)?
    }

    #[cfg(test)]
    pub fn subscribe(&self) -> AgentProgressSubscription {
        self.subscribe_to(None)
    }

    pub fn subscribe_run(&self, run_id: AgentRunId) -> AgentProgressSubscription {
        self.subscribe_to(Some(run_id))
    }

    fn subscribe_to(&self, run_id: Option<AgentRunId>) -> AgentProgressSubscription {
        let (sender, receiver) = tokio::sync::mpsc::channel(PROGRESS_CAPACITY);
        let lagged = Arc::new(AtomicBool::new(false));
        self.progress
            .lock()
            .expect("Agent progress lock poisoned")
            .push(ProgressSubscriber {
                run_id,
                sender,
                lagged: lagged.clone(),
            });
        AgentProgressSubscription { receiver, lagged }
    }
}

pub struct AgentService {
    sender: mpsc::SyncSender<Command>,
    worker: Option<thread::JoinHandle<()>>,
}

impl AgentService {
    pub fn start(
        dependencies: AgentDependencies,
        extensions: Vec<ExtensionBindings>,
    ) -> Result<(Self, AgentHandle), AgentServiceError> {
        let progress = Arc::new(Mutex::new(Vec::new()));
        let (sender, receiver) = mpsc::sync_channel(SCHEDULER_QUEUE_CAPACITY);
        let tools = prepare_tools(extensions)?;
        let recoverable = load_recoverable_runs(&dependencies, &tools)?;
        let worker_progress = progress.clone();
        let worker = thread::Builder::new()
            .name("agl-scheduler".into())
            .spawn({
                let scheduler = sender.clone();
                move || {
                    worker_loop(
                        receiver,
                        scheduler,
                        dependencies,
                        tools,
                        worker_progress,
                        recoverable,
                    )
                }
            })
            .map_err(|_| AgentServiceError::Unavailable)?;
        let handle = AgentHandle {
            sender: sender.clone(),
            progress,
        };
        Ok((
            Self {
                sender,
                worker: Some(worker),
            },
            handle,
        ))
    }

    pub fn shutdown(mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for AgentService {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
