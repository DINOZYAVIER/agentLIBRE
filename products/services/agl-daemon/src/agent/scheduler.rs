use super::*;

pub(crate) enum Command {
    StartRun {
        spec: AgentRunSpec,
        reply: mpsc::SyncSender<Result<AgentRunId, AgentServiceError>>,
    },
    CancelRun {
        run_id: AgentRunId,
        reply: mpsc::SyncSender<Result<(), AgentServiceError>>,
    },
    RunFinished {
        run_id: AgentRunId,
        retry: bool,
        cancellation: RunCancellation,
    },
    Shutdown,
}

struct QueuedRun {
    run: AgentRun,
    cancellation: RunCancellation,
}

pub(crate) fn prepare_tools(
    extensions: Vec<ExtensionBindings>,
) -> Result<BTreeMap<String, PreparedTool>, AgentServiceError> {
    let mut tools = BTreeMap::new();
    for extension in extensions {
        extension
            .definition
            .validate()
            .map_err(|_| AgentServiceError::InvalidBindings)?;
        if extension.tools.len() != extension.definition.tools.len() {
            return Err(AgentServiceError::InvalidBindings);
        }
        let effects = extension
            .definition
            .effects
            .iter()
            .map(|effect| (effect.id.clone(), effect.scope_schema.clone()))
            .collect::<BTreeMap<_, _>>();
        let effect_validators = effects
            .iter()
            .map(|(id, schema)| {
                jsonschema::validator_for(schema.as_value())
                    .map(|validator| (id.clone(), Arc::new(validator)))
                    .map_err(|_| AgentServiceError::InvalidBindings)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let extension_digest = ExtensionDefinitionDigest::from_bytes(sha256(
            &serde_json::to_vec(&extension.definition)
                .map_err(|_| AgentServiceError::InvalidBindings)?,
        ));
        for tool in extension.tools {
            if !tool
                .tool_id
                .as_str()
                .starts_with(&format!("{}:", extension.definition.id.as_str()))
            {
                return Err(AgentServiceError::InvalidBindings);
            }
            let definition = extension
                .definition
                .tools
                .iter()
                .find(|definition| definition.id == tool.tool_id)
                .cloned()
                .ok_or(AgentServiceError::InvalidBindings)?;
            let actual_digest = ToolDefinitionDigest::from_bytes(sha256(
                &serde_json::to_vec(&definition).map_err(|_| AgentServiceError::InvalidBindings)?,
            ));
            let input_validator = Arc::new(
                jsonschema::validator_for(definition.input_schema.as_value())
                    .map_err(|_| AgentServiceError::InvalidBindings)?,
            );
            if actual_digest != tool.definition_digest {
                return Err(AgentServiceError::InvalidBindings);
            }
            if tools
                .insert(
                    tool.tool_id.as_str().to_owned(),
                    PreparedTool {
                        binding: tool,
                        definition,
                        input_validator,
                        extension_id: extension.definition.id.clone(),
                        extension_version: extension.version.clone(),
                        extension_digest,
                        effect_validators: effect_validators.clone(),
                    },
                )
                .is_some()
            {
                return Err(AgentServiceError::InvalidBindings);
            }
        }
    }
    Ok(tools)
}

pub(crate) fn worker_loop(
    receiver: mpsc::Receiver<Command>,
    scheduler: mpsc::SyncSender<Command>,
    dependencies: AgentDependencies,
    tools: BTreeMap<String, PreparedTool>,
    progress: ProgressSubscribers,
    recoverable: Vec<AgentRun>,
) {
    let tools = Arc::new(tools);
    let mut active = BTreeMap::new();
    let mut queued = VecDeque::new();
    let mut workers = Vec::new();
    for run in recoverable {
        queued.push_back(QueuedRun {
            run,
            cancellation: RunCancellation::default(),
        });
    }
    start_queued_runs(
        &mut queued,
        &mut active,
        &mut workers,
        &dependencies,
        &tools,
        &progress,
        &scheduler,
    );
    let mut shutting_down = false;
    loop {
        let command = match receiver.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !shutting_down {
                    start_queued_runs(
                        &mut queued,
                        &mut active,
                        &mut workers,
                        &dependencies,
                        &tools,
                        &progress,
                        &scheduler,
                    );
                }
                reap_workers(&mut workers);
                if shutting_down && active.is_empty() {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match command {
            Command::StartRun { reply, .. } if shutting_down => {
                let _ = reply.send(Err(AgentServiceError::Stopped));
            }
            Command::CancelRun { reply, .. } if shutting_down => {
                let _ = reply.send(Err(AgentServiceError::Stopped));
            }
            Command::StartRun { spec, reply } => match admit(
                &dependencies,
                &tools,
                spec,
                queued.len() < SCHEDULER_QUEUE_CAPACITY,
            ) {
                Ok(run) => {
                    tracing::info!(run_id=%run.id, "AgentRun admitted");
                    let _ = reply.send(Ok(run.id));
                    if !run.status.is_terminal()
                        && !active.contains_key(&run.id)
                        && !queued.iter().any(|candidate| candidate.run.id == run.id)
                    {
                        queued.push_back(QueuedRun {
                            run,
                            cancellation: RunCancellation::default(),
                        });
                        start_queued_runs(
                            &mut queued,
                            &mut active,
                            &mut workers,
                            &dependencies,
                            &tools,
                            &progress,
                            &scheduler,
                        );
                    }
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Command::CancelRun { run_id, reply } => {
                let result = if let Some(cancellation) = active.get(&run_id) {
                    cancellation.cancel();
                    Ok(())
                } else if let Some(queued) =
                    queued.iter().find(|candidate| candidate.run.id == run_id)
                {
                    queued.cancellation.cancel();
                    Ok(())
                } else {
                    dependencies
                        .store
                        .agent_run_view(run_id)
                        .map_err(AgentServiceError::from)
                        .and_then(|view| {
                            if view.status.is_terminal() {
                                Ok(())
                            } else {
                                // A non-terminal Run without a worker is a failed scheduler
                                // invariant, not a second cancellation persistence path.
                                Err(AgentServiceError::Unavailable)
                            }
                        })
                };
                let _ = reply.send(result);
            }
            Command::RunFinished {
                run_id,
                retry,
                cancellation,
            } => {
                active.remove(&run_id);
                if retry && !shutting_down {
                    match dependencies.store.agent_run(run_id) {
                        Ok(run) if !run.status.is_terminal() => {
                            if queued.len() == SCHEDULER_QUEUE_CAPACITY {
                                queued.pop_back();
                            }
                            queued.push_front(QueuedRun { run, cancellation });
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::error!(%run_id, %error, "failed to reload interrupted AgentRun");
                        }
                    }
                }
                if !shutting_down {
                    if let Err(error) =
                        refill_recoverable_runs(&mut queued, &active, &dependencies, &tools)
                    {
                        tracing::error!(%error, "failed to refill recoverable AgentRun queue");
                    }
                    start_queued_runs(
                        &mut queued,
                        &mut active,
                        &mut workers,
                        &dependencies,
                        &tools,
                        &progress,
                        &scheduler,
                    );
                }
            }
            Command::Shutdown => {
                shutting_down = true;
                for cancellation in active.values() {
                    cancellation.cancel();
                }
            }
        }
        reap_workers(&mut workers);
        if shutting_down && active.is_empty() {
            break;
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
}

fn spawn_run(
    dependencies: AgentDependencies,
    tools: Arc<BTreeMap<String, PreparedTool>>,
    progress: ProgressSubscribers,
    run: AgentRun,
    cancellation: RunCancellation,
    scheduler: mpsc::SyncSender<Command>,
) -> Result<thread::JoinHandle<()>, AgentServiceError> {
    thread::Builder::new()
        .name(format!("agl-run-{}", run.id))
        .spawn(move || {
            let run_id = run.id;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drive(
                    &dependencies,
                    &tools,
                    runtime.as_ref(),
                    &progress,
                    &cancellation,
                    run,
                )
            }))
            .unwrap_or(Err(AgentServiceError::Unavailable));
            let outcome = outcome.and_then(|()| {
                let view = dependencies.store.agent_run_view(run_id)?;
                if !view.status.is_terminal() {
                    return Err(AgentServiceError::InvalidTransition);
                }
                tracing::info!(
                    %run_id,
                    status=?view.status,
                    failure=?view.failure,
                    model_input_tokens=view.usage.model_input_tokens,
                    model_output_tokens=view.usage.model_output_tokens,
                    model_calls=view.usage.model_calls,
                    tool_calls=view.usage.tool_calls,
                    "AgentRun driver reached a terminal state"
                );
                Ok(())
            });
            let retry = outcome.as_ref().is_err_and(retry_scheduler_failure);
            if let Err(error) = &outcome {
                tracing::error!(%run_id, %error, retry, "AgentRun driver stopped before a terminal state");
            }
            if retry {
                thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = scheduler.send(Command::RunFinished {
                run_id,
                retry,
                cancellation,
            });
        })
        .map_err(|_| AgentServiceError::Unavailable)
}

fn start_queued_runs(
    queued: &mut VecDeque<QueuedRun>,
    active: &mut BTreeMap<AgentRunId, RunCancellation>,
    workers: &mut Vec<thread::JoinHandle<()>>,
    dependencies: &AgentDependencies,
    tools: &Arc<BTreeMap<String, PreparedTool>>,
    progress: &ProgressSubscribers,
    scheduler: &mpsc::SyncSender<Command>,
) {
    while active.len() < MAX_ACTIVE_RUNS {
        let Some(queued_run) = queued.pop_front() else {
            break;
        };
        let QueuedRun { run, cancellation } = queued_run;
        let run_id = run.id;
        let worker = spawn_run(
            dependencies.clone(),
            tools.clone(),
            progress.clone(),
            run,
            cancellation.clone(),
            scheduler.clone(),
        );
        match worker {
            Ok(worker) => {
                active.insert(run_id, cancellation);
                workers.push(worker);
            }
            Err(error) => {
                tracing::error!(%run_id, %error, "failed to spawn AgentRun driver");
                queued.push_front(QueuedRun {
                    run: match dependencies.store.agent_run(run_id) {
                        Ok(run) => run,
                        Err(store_error) => {
                            tracing::error!(%run_id, %store_error, "failed to reload unstarted AgentRun");
                            break;
                        }
                    },
                    cancellation,
                });
                break;
            }
        }
    }
}

pub(crate) fn load_recoverable_runs(
    dependencies: &AgentDependencies,
    tools: &BTreeMap<String, PreparedTool>,
) -> Result<Vec<AgentRun>, AgentServiceError> {
    let mut offset = 0;
    let mut scheduled = Vec::with_capacity(RECOVERY_PAGE_CAPACITY);
    loop {
        let page = dependencies
            .store
            .recoverable_agent_runs(offset, RECOVERY_PAGE_CAPACITY)?;
        let page_len = page.len();
        for run in page {
            validate_snapshot(&run.snapshot, tools)?;
            if scheduled.len() < RECOVERY_PAGE_CAPACITY {
                scheduled.push(run);
            }
        }
        if page_len < RECOVERY_PAGE_CAPACITY {
            break;
        }
        offset = offset
            .checked_add(page_len)
            .ok_or(AgentServiceError::Unavailable)?;
    }
    Ok(scheduled)
}

fn refill_recoverable_runs(
    queued: &mut VecDeque<QueuedRun>,
    active: &BTreeMap<AgentRunId, RunCancellation>,
    dependencies: &AgentDependencies,
    tools: &BTreeMap<String, PreparedTool>,
) -> Result<(), AgentServiceError> {
    if queued.len() >= SCHEDULER_QUEUE_CAPACITY {
        return Ok(());
    }
    for run in dependencies
        .store
        .recoverable_agent_runs(0, RECOVERY_PAGE_CAPACITY)?
    {
        if queued.len() >= SCHEDULER_QUEUE_CAPACITY {
            break;
        }
        if active.contains_key(&run.id) || queued.iter().any(|candidate| candidate.run.id == run.id)
        {
            continue;
        }
        validate_snapshot(&run.snapshot, tools)?;
        queued.push_back(QueuedRun {
            run,
            cancellation: RunCancellation::default(),
        });
    }
    Ok(())
}

fn retry_scheduler_failure(error: &AgentServiceError) -> bool {
    matches!(
        error,
        AgentServiceError::Unavailable
            | AgentServiceError::Store(
                crate::store::StoreError::Io(_)
                    | crate::store::StoreError::Sqlite(_)
                    | crate::store::StoreError::TransitionRejected { .. }
            )
    )
}

pub(crate) fn map_command_send(error: mpsc::TrySendError<Command>) -> AgentServiceError {
    match error {
        mpsc::TrySendError::Full(_) => AgentServiceError::Unavailable,
        mpsc::TrySendError::Disconnected(_) => AgentServiceError::Stopped,
    }
}

fn reap_workers(workers: &mut Vec<thread::JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            let _ = worker.join();
        } else {
            index += 1;
        }
    }
}
