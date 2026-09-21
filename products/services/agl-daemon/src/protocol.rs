use super::*;
use std::path::Path;

pub async fn serve_connection(stream: UnixStream, handle: DaemonHandle) -> Result<()> {
    verify_peer(&stream)?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let Some(frame) = read_bounded_line(&mut reader).await? else {
        return Ok(());
    };
    let request: AgentProtocolRequest =
        serde_json::from_slice(&frame).context("invalid Agent request")?;
    let response_request_id = request.request_id.clone();
    let response = match request.validate() {
        Ok(()) => {
            if let AgentCommand::Subscribe { run_id } = &request.command {
                return serve_subscription(writer, handle, request.request_id.clone(), *run_id)
                    .await;
            }
            dispatch(request, handle).await
        }
        Err(error) => AgentProtocolResponse::error(request.request_id, error),
    };
    let mut response = serde_json::to_vec(&response).context("failed to encode Agent response")?;
    if response.len() > MAX_JSONL_FRAME_BYTES {
        response = serde_json::to_vec(&AgentProtocolResponse::error(
            response_request_id,
            ProtocolError::new(
                ProtocolErrorCode::FrameTooLarge,
                "Agent response exceeds the 8 MiB frame limit",
                false,
            ),
        ))?;
    }
    response.push(b'\n');
    writer.write_all(&response).await?;
    Ok(())
}

async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            anyhow::ensure!(line.is_empty(), "Agent frame is not newline-terminated");
            return Ok(None);
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        anyhow::ensure!(
            line.len().saturating_add(consumed) <= MAX_JSONL_FRAME_BYTES + 1,
            "Agent frame exceeds the 8 MiB limit"
        );
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

async fn serve_subscription(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    handle: DaemonHandle,
    request_id: agl_core::RequestId,
    run_id: agl_core::AgentRunId,
) -> Result<()> {
    // Register transient delivery first; the Store cursor read that follows is
    // the durable half of the race-free subscription boundary.
    let mut progress = handle.agent.subscribe_run(run_id);
    let store = handle.store.clone();
    let view = tokio::task::spawn_blocking(move || store.agent_run_view(run_id)).await??;
    let mut cursor = view.last_event_id;
    write_stream_frame(
        &mut writer,
        AgentProtocolStreamFrame::new(
            request_id.clone(),
            AgentSubscriptionFrame::Subscribed {
                run_view: view.clone(),
                cursor,
            },
        ),
    )
    .await?;
    if view.status.is_terminal() {
        write_stream_frame(
            &mut writer,
            AgentProtocolStreamFrame::new(
                request_id,
                AgentSubscriptionFrame::Ended {
                    run_view: view,
                    cursor,
                },
            ),
        )
        .await?;
        return Ok(());
    }

    loop {
        let store = handle.store.clone();
        let page = tokio::task::spawn_blocking(move || store.agent_event_page(Some(cursor), 256))
            .await??;
        let had_events = !page.events.is_empty();
        let mut terminal = false;
        for event in page.events {
            cursor = event.id;
            if event.agent_run_id == run_id {
                terminal |= matches!(
                    &event.data,
                    agl_core::agent::AgentEventData::RunStatusChanged { to, .. }
                        if to.is_terminal()
                );
                write_stream_frame(
                    &mut writer,
                    AgentProtocolStreamFrame::new(
                        request_id.clone(),
                        AgentSubscriptionFrame::Event { event },
                    ),
                )
                .await?;
            }
        }
        if terminal {
            let store = handle.store.clone();
            let view = tokio::task::spawn_blocking(move || store.agent_run_view(run_id)).await??;
            write_stream_frame(
                &mut writer,
                AgentProtocolStreamFrame::new(
                    request_id,
                    AgentSubscriptionFrame::Ended {
                        run_view: view,
                        cursor,
                    },
                ),
            )
            .await?;
            return Ok(());
        }
        if progress.take_lagged() {
            while progress.try_recv().is_ok() {}
            write_stream_frame(
                &mut writer,
                AgentProtocolStreamFrame::new(
                    request_id.clone(),
                    AgentSubscriptionFrame::Lagged {
                        last_durable_cursor: cursor,
                    },
                ),
            )
            .await?;
            continue;
        }
        while let Ok(signal) = progress.try_recv() {
            let crate::agent::SubscriptionSignal::Progress(update) = signal else {
                continue;
            };
            let operation = match &update {
                agl_daemon_api::AgentProgress::ModelOutputDelta { operation, .. }
                | agl_daemon_api::AgentProgress::OperationStatus { operation, .. } => operation,
            };
            if operation.run_id == run_id {
                write_stream_frame(
                    &mut writer,
                    AgentProtocolStreamFrame::new(
                        request_id.clone(),
                        AgentSubscriptionFrame::Progress { progress: update },
                    ),
                )
                .await?;
            }
        }
        if !had_events {
            let Some(signal) = progress.recv().await else {
                return Ok(());
            };
            if let crate::agent::SubscriptionSignal::Progress(update) = signal {
                write_stream_frame(
                    &mut writer,
                    AgentProtocolStreamFrame::new(
                        request_id.clone(),
                        AgentSubscriptionFrame::Progress { progress: update },
                    ),
                )
                .await?;
            }
        }
    }
}

async fn write_stream_frame(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    frame: AgentProtocolStreamFrame,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(&frame)?;
    anyhow::ensure!(
        bytes.len() <= MAX_JSONL_FRAME_BYTES,
        "stream frame is oversized"
    );
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

async fn dispatch(request: AgentProtocolRequest, handle: DaemonHandle) -> AgentProtocolResponse {
    let request_id = request.request_id;
    let result = match request.command {
        AgentCommand::OpenConversation {
            conversation_id,
            function_path,
            workspace_path,
        } => {
            let store = handle.store.clone();
            let existing =
                tokio::task::spawn_blocking(move || store.conversation_binding(conversation_id))
                    .await
                    .map_err(protocol_join_error);
            match existing {
                Ok(Ok(_)) => Err(ProtocolError::new(
                    ProtocolErrorCode::Conflict,
                    format!("Conversation {conversation_id} is already bound"),
                    false,
                )),
                Ok(Err(crate::store::StoreError::NotFound { .. })) => {
                    let activated = handle
                        .runtime
                        .activate(PathBuf::from(function_path), PathBuf::from(workspace_path))
                        .await
                        .map_err(protocol_activation_error);
                    match activated {
                        Ok(activated) => {
                            let function = ExactPackageRef {
                                id: activated.function_id,
                                version: activated.function_version,
                                digest: activated.function_digest,
                            };
                            let activation = FunctionActivationView {
                                function: function.clone(),
                                dependencies: activated.progress.dependencies,
                                model_artifact: activated.progress.model_artifact,
                                model_service: activated.progress.model_service,
                                runtime_profile: activated.progress.runtime_profile,
                                active_slots: activated.progress.active_slots,
                                continuous_batching: activated.progress.continuous_batching,
                            };
                            let store = handle.store.clone();
                            let snapshot = activated.snapshot;
                            tokio::task::spawn_blocking(move || {
                                store.create_conversation(conversation_id, &function, &snapshot)
                            })
                            .await
                            .map_err(protocol_join_error)
                            .and_then(|result| result.map_err(protocol_store_error))
                            .map(|conversation| {
                                AgentResponse::ConversationOpened {
                                    conversation,
                                    activation: Box::new(activation),
                                }
                            })
                        }
                        Err(error) => Err(error),
                    }
                }
                Ok(Err(error)) => Err(protocol_store_error(error)),
                Err(error) => Err(error),
            }
        }
        AgentCommand::PlanCreate {
            function_path,
            workspace_path,
            prompt,
        } => {
            let activated = handle
                .runtime
                .activate(PathBuf::from(function_path), PathBuf::from(&workspace_path))
                .await
                .map_err(protocol_activation_error);
            match activated {
                Ok(activated) => {
                    let function = ExactPackageRef {
                        id: activated.function_id.clone(),
                        version: activated.function_version.clone(),
                        digest: activated.function_digest,
                    };
                    let workspace = PathBuf::from(workspace_path)
                        .canonicalize()
                        .map_err(|error| {
                            ProtocolError::new(
                                ProtocolErrorCode::InvalidRequest,
                                format!("failed to canonicalize planner workspace: {error}"),
                                false,
                            )
                        });
                    match workspace {
                        Ok(workspace) => {
                            let plans = handle.plans.clone();
                            let agent = handle.agent.clone();
                            let store = handle.store.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                plans.create_from_conversation(
                                    &agent, &store, &function, &activated, &workspace, prompt,
                                )
                            })
                            .await
                            .map_err(protocol_join_error)
                            .and_then(|result| {
                                result.map_err(|error| {
                                    ProtocolError::new(
                                        ProtocolErrorCode::InvalidRequest,
                                        error.to_string(),
                                        false,
                                    )
                                })
                            });
                            result.map(|(stored, conversation)| AgentResponse::PlanCreated {
                                plan: Box::new(PlanArtifactView {
                                    state: agl_core::implementation_plan::PlanState::for_draft(
                                        &stored.plan,
                                    ),
                                    plan: stored.plan,
                                    digest: stored.digest,
                                    results: Vec::new(),
                                    slices: Vec::new(),
                                }),
                                conversation,
                            })
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        }
        AgentCommand::PlanView { plan_id } => {
            let plans = handle.plans.clone();
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || plans.view(&store, &plan_id))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| {
                    result
                        .map(|plan| AgentResponse::PlanViewed {
                            plan: Box::new(plan),
                        })
                        .map_err(|error| {
                            ProtocolError::new(
                                ProtocolErrorCode::NotFound,
                                error.to_string(),
                                false,
                            )
                        })
                })
        }
        AgentCommand::PlanApprove {
            plan_id,
            expected_digest,
        } => {
            let plans = handle.plans.clone();
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || {
                plans.approve_draft_durable(&store, &plan_id, &expected_digest)
            })
            .await
            .map_err(protocol_join_error)
            .and_then(|result| {
                result
                    .map(|plan| AgentResponse::PlanApproved {
                        plan: Box::new(plan),
                    })
                    .map_err(|error| {
                        ProtocolError::new(ProtocolErrorCode::Conflict, error.to_string(), false)
                    })
            })
        }
        AgentCommand::PlanImplement {
            plan_id,
            expected_digest,
            function_path,
        } => {
            let plans = handle.plans.clone();
            let store = handle.store.clone();
            let agent = handle.agent.clone();
            let runtime = handle.runtime.clone();
            let execution = handle.execution.clone();
            tokio::task::spawn_blocking(move || {
                plans.implement(
                    &store,
                    &agent,
                    &runtime,
                    &execution,
                    &plan_id,
                    &expected_digest,
                    Path::new(&function_path),
                )
            })
            .await
            .map_err(protocol_join_error)
            .and_then(|result| {
                result.map_err(|error| {
                    ProtocolError::new(ProtocolErrorCode::Conflict, error.to_string(), false)
                })
            })
            .map(|plan| AgentResponse::PlanImplementStarted {
                plan: Box::new(plan),
            })
        }
        AgentCommand::PlanStatus { plan_id } => {
            let plans = handle.plans.clone();
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || plans.view(&store, &plan_id))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| {
                    result.map_err(|error| {
                        ProtocolError::new(ProtocolErrorCode::NotFound, error.to_string(), false)
                    })
                })
                .map(|plan| AgentResponse::PlanStatus {
                    plan: Box::new(plan),
                })
        }
        AgentCommand::Conversations {
            workspace_path,
            limit,
        } => {
            let workspace = workspace_path
                .map(PathBuf::from)
                .map(|path| {
                    path.canonicalize()
                        .context("failed to canonicalize Conversation workspace filter")
                })
                .transpose()
                .map_err(protocol_activation_error);
            match workspace {
                Ok(workspace) => {
                    let store = handle.store.clone();
                    tokio::task::spawn_blocking(move || {
                        store.conversations(workspace.as_deref(), usize::from(limit))
                    })
                    .await
                    .map_err(protocol_join_error)
                    .and_then(|result| result.map_err(protocol_store_error))
                    .map(|conversations| AgentResponse::Conversations { conversations })
                }
                Err(error) => Err(error),
            }
        }
        AgentCommand::ResolveConversation { selector } => {
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || store.resolve_conversation(&selector))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| result.map_err(protocol_store_error))
                .map(|conversation| AgentResponse::ConversationResolved { conversation })
        }
        AgentCommand::RenameConversation {
            selector,
            display_name,
        } => {
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || store.rename_conversation(&selector, &display_name))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| result.map_err(protocol_store_error))
                .map(|conversation| AgentResponse::ConversationRenamed { conversation })
        }
        AgentCommand::StartRun { spec } => {
            let agent = handle.agent.clone();
            tokio::task::spawn_blocking(move || agent.start_run(spec))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| result.map_err(protocol_agent_error))
                .map(|run_id| AgentResponse::RunStarted { run_id })
        }
        AgentCommand::CancelRun { run_id } => {
            let agent = handle.agent.clone();
            tokio::task::spawn_blocking(move || agent.cancel(run_id))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| result.map_err(protocol_agent_error))
                .map(|()| AgentResponse::RunCancelled { run_id })
        }
        AgentCommand::RunView { run_id } => {
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || store.agent_run_view(run_id))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| result.map_err(protocol_store_error))
                .map(|view| AgentResponse::RunView { view })
        }
        AgentCommand::OperationView { key } => {
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || store.agent_operation(&key))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| result.map_err(protocol_store_error))
                .map(|operation| AgentResponse::OperationView {
                    operation: operation.into(),
                })
        }
        AgentCommand::Events { after, limit } => {
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || store.agent_event_page(after, usize::from(limit)))
                .await
                .map_err(protocol_join_error)
                .and_then(|result| result.map_err(protocol_store_error))
                .map(|page| AgentResponse::Events { page })
        }
        AgentCommand::Messages {
            conversation_id,
            after,
            limit,
        } => {
            let store = handle.store.clone();
            tokio::task::spawn_blocking(move || {
                store.conversation_messages(conversation_id, after.as_ref(), usize::from(limit))
            })
            .await
            .map_err(protocol_join_error)
            .and_then(|result| result.map_err(protocol_store_error))
            .map(|page| AgentResponse::Messages { page })
        }
        AgentCommand::Subscribe { .. } => Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "Subscribe requires a streaming connection",
            false,
        )),
    };
    match result {
        Ok(response) => AgentProtocolResponse::ok(request_id, response),
        Err(error) => AgentProtocolResponse::error(request_id, error),
    }
}

fn protocol_activation_error(error: anyhow::Error) -> ProtocolError {
    use agl_runtime::inference::InferenceServiceError;

    let (code, retryable) = match error.downcast_ref::<InferenceServiceError>() {
        Some(
            InferenceServiceError::Stopped
            | InferenceServiceError::Unavailable
            | InferenceServiceError::UnavailableWithReason(_)
            | InferenceServiceError::DeviceLost,
        ) => (ProtocolErrorCode::Unavailable, true),
        Some(InferenceServiceError::InvalidRequest | InferenceServiceError::IdentityMismatch) => {
            (ProtocolErrorCode::InvalidRequest, false)
        }
        Some(InferenceServiceError::ContextExhausted(_)) => (ProtocolErrorCode::Conflict, false),
        Some(
            InferenceServiceError::Cancelled
            | InferenceServiceError::Deadline
            | InferenceServiceError::OutcomeUnknown
            | InferenceServiceError::InvalidResult
            | InferenceServiceError::InvalidModelOutput(_),
        ) => (ProtocolErrorCode::Internal, false),
        None => (ProtocolErrorCode::InvalidRequest, false),
    };
    let message = error.to_string().chars().take(512).collect::<String>();
    ProtocolError::new(code, message, retryable)
}

fn protocol_join_error(error: tokio::task::JoinError) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, error.to_string(), false)
}

fn protocol_agent_error(error: crate::agent::AgentServiceError) -> ProtocolError {
    let message = error.to_string();
    let code = match error {
        crate::agent::AgentServiceError::Stopped | crate::agent::AgentServiceError::Unavailable => {
            ProtocolErrorCode::Unavailable
        }
        crate::agent::AgentServiceError::InvalidTransition
        | crate::agent::AgentServiceError::Fsm(_) => ProtocolErrorCode::Conflict,
        crate::agent::AgentServiceError::Store(store) => return protocol_store_error(store),
        crate::agent::AgentServiceError::InvalidBindings
        | crate::agent::AgentServiceError::InvalidSnapshot => ProtocolErrorCode::InvalidRequest,
    };
    ProtocolError::new(code, message, code == ProtocolErrorCode::Unavailable)
}

fn protocol_store_error(error: crate::store::StoreError) -> ProtocolError {
    let code = match &error {
        crate::store::StoreError::ConversationBusy { .. } => ProtocolErrorCode::Busy,
        crate::store::StoreError::NotFound { .. } => ProtocolErrorCode::NotFound,
        crate::store::StoreError::TransitionRejected { .. } => ProtocolErrorCode::Conflict,
        crate::store::StoreError::InvalidPath { .. }
        | crate::store::StoreError::InvalidValue { .. }
        | crate::store::StoreError::Content(_) => ProtocolErrorCode::InvalidRequest,
        crate::store::StoreError::IncompatibleDatabase { .. }
        | crate::store::StoreError::Busy { .. }
        | crate::store::StoreError::Io(_)
        | crate::store::StoreError::Sqlite(_)
        | crate::store::StoreError::Json(_) => ProtocolErrorCode::Internal,
    };
    ProtocolError::new(code, error.to_string(), false)
}

fn verify_peer(stream: &UnixStream) -> Result<()> {
    let peer = stream.peer_cred()?;
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let expected = unsafe { libc::geteuid() };
    anyhow::ensure!(
        peer.uid() == expected,
        "client socket UID differs from daemon UID"
    );
    Ok(())
}

#[cfg(test)]
mod mapping_tests {
    use super::*;
    use agl_runtime::inference::InferenceServiceError;

    #[test]
    fn activation_inference_causes_have_stable_codes_and_retryability() {
        for (cause, code, retryable) in [
            (
                InferenceServiceError::InvalidRequest,
                ProtocolErrorCode::InvalidRequest,
                false,
            ),
            (
                InferenceServiceError::ContextExhausted(agl_core::agent::ContextCapacity::new(
                    10, 10, 8,
                )),
                ProtocolErrorCode::Conflict,
                false,
            ),
            (
                InferenceServiceError::Unavailable,
                ProtocolErrorCode::Unavailable,
                true,
            ),
            (
                InferenceServiceError::UnavailableWithReason(
                    "resident-memory admission rejected".to_owned(),
                ),
                ProtocolErrorCode::Unavailable,
                true,
            ),
            (
                InferenceServiceError::OutcomeUnknown,
                ProtocolErrorCode::Internal,
                false,
            ),
        ] {
            let mapped = protocol_activation_error(anyhow::Error::new(cause));
            assert_eq!(mapped.code, code);
            assert_eq!(mapped.retryable, retryable);
        }
    }

    #[test]
    fn activation_unavailable_reason_reaches_the_client_message() {
        let mapped = protocol_activation_error(anyhow::Error::new(
            InferenceServiceError::UnavailableWithReason(
                "resident-memory admission rejected".to_owned(),
            ),
        ));
        assert_eq!(
            mapped.message,
            "inference is unavailable: resident-memory admission rejected"
        );
    }

    #[test]
    fn activation_diagnostic_is_bounded() {
        let mapped = protocol_activation_error(anyhow::anyhow!("x".repeat(600)));
        assert_eq!(mapped.message.chars().count(), 512);
    }
}
