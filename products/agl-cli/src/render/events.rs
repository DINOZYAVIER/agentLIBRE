use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn foreground_turn(
    client: &AgentClient,
    conversation_id: ConversationId,
    prompt: String,
    interactive: bool,
    reasoning: Option<agl_core::agent::ReasoningEffort>,
    decorations: Decorations,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    theme: &TerminalTheme,
    model_generation_details: bool,
    active_id: Option<&Arc<Mutex<Option<AgentRunId>>>>,
) -> Result<agl_core::agent::AgentRunView> {
    let started = Instant::now();
    let message_id = MessageId::generate();
    let run_id = client
        .start_run(AgentRunSpec {
            reasoning,
            origin: AgentRunOrigin::User {
                conversation_id,
                message_id: message_id.clone(),
            },
            input: Content::text(prompt)?,
        })
        .await?;
    if let Some(active_id) = active_id {
        *active_id.lock().expect("active run lock poisoned") = Some(run_id);
    }
    if interactive && decorations != Decorations::Off {
        begin_external_block();
        println!(
            "{} {}",
            theme.paint("run", "RUN"),
            theme.paint("run_id", &run_id.to_string())
        );
    } else if interactive {
        eprintln!("run {run_id}");
    }
    let (view, streamed) = stream_until_terminal(
        client,
        run_id,
        interactive,
        decorations,
        tool_output,
        tool_frame,
        theme,
        model_generation_details,
        started,
    )
    .await?;
    if view.status == AgentRunStatus::Completed {
        let page = client
            .messages(conversation_id, Some(message_id), 1_000)
            .await?;
        if let Some(message) = page.messages.into_iter().find(|message| {
            message.run_id == Some(run_id) && message.role == MessageRole::Assistant
        }) {
            let durable = message.content.into_text();
            if decorations != Decorations::Off && interactive {
                render_answer_block(&durable, started.elapsed(), theme);
            } else if !interactive || streamed != durable {
                println!("{}", safe_terminal_text(&durable));
            }
        }
    } else if interactive && !streamed.is_empty() {
        println!("\n[ответ прерван]");
    }
    if interactive && decorations != Decorations::Off {
        let status = format!("{:?}", view.status);
        println!(
            "{} {}",
            theme.paint("run", "RUN"),
            theme.paint(status_role(view.status), &status)
        );
    }
    if interactive && decorations == Decorations::Full {
        let status = format!("{:?}", view.status);
        println!(
            "{} {}  {}  {}",
            theme.paint("run", "RUN SUMMARY"),
            theme.paint("run_id", &run_id.to_string()),
            theme.paint(status_role(view.status), &status),
            theme.paint("muted", &format!("{} ms", started.elapsed().as_millis()))
        );
    } else if interactive && decorations == Decorations::Off {
        eprintln!(
            "run_id={run_id} status={:?} elapsed_ms={}",
            view.status,
            started.elapsed().as_millis()
        );
    }
    Ok(view)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_until_terminal(
    client: &AgentClient,
    run_id: AgentRunId,
    render: bool,
    decorations: Decorations,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    theme: &TerminalTheme,
    model_generation_details: bool,
    started: Instant,
) -> Result<(agl_core::agent::AgentRunView, String)> {
    let decorated = decorations != Decorations::Off;
    let mut subscription = client.subscribe(run_id).await?;
    if subscription.initial_view().status.is_terminal() {
        return Ok((subscription.initial_view().clone(), String::new()));
    }
    let mut streamed = String::new();
    let mut streaming_region_active = false;
    let mut operation_started_at_ms = BTreeMap::new();
    let mut cancellation_requested = false;
    loop {
        let frame = if cancellation_requested {
            subscription.next().await?
        } else {
            tokio::select! {
                frame = subscription.next() => frame?,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl-C")?;
                    if let Err(error) = client.cancel(run_id).await {
                        let view = client.run_view(run_id).await.with_context(|| {
                            format!("cancellation of AgentRun {run_id} failed ({error}); state is unknown")
                        })?;
                        if view.status.is_terminal() {
                            return Ok((view, streamed));
                        }
                        return Err(error).with_context(|| {
                            format!("could not confirm cancellation for AgentRun {run_id}; it is still active")
                        });
                    }
                    cancellation_requested = true;
                    finish_streaming_region(
                        render,
                        decorated,
                        &mut streaming_region_active,
                        theme,
                        started.elapsed(),
                    )?;
                    if render && decorated {
                        println!("cancellation requested");
                    } else if render {
                        eprintln!("cancellation requested");
                    }
                    continue;
                }
            }
        };
        match frame {
            AgentSubscriptionFrame::Progress { progress } => {
                render_progress(
                    progress,
                    render,
                    decorated,
                    &mut streamed,
                    &mut streaming_region_active,
                    started,
                    theme,
                )?;
            }
            AgentSubscriptionFrame::Event { event } => {
                finish_streaming_region(
                    render,
                    decorated,
                    &mut streaming_region_active,
                    theme,
                    started.elapsed(),
                )?;
                let terminal = event_ends_run(&event);
                render_event(
                    client,
                    &event,
                    render,
                    decorated,
                    tool_output,
                    tool_frame,
                    &mut operation_started_at_ms,
                    theme,
                    model_generation_details,
                )
                .await;
                if terminal {
                    return Ok((client.run_view(run_id).await?, streamed));
                }
            }
            AgentSubscriptionFrame::Lagged { .. } => {
                finish_streaming_region(
                    render,
                    decorated,
                    &mut streaming_region_active,
                    theme,
                    started.elapsed(),
                )?;
                let snapshot = subscription.resynchronize(client).await?;
                for event in &snapshot.events {
                    render_event(
                        client,
                        event,
                        render,
                        decorated,
                        tool_output,
                        tool_frame,
                        &mut operation_started_at_ms,
                        theme,
                        model_generation_details,
                    )
                    .await;
                }
                if snapshot.view.status.is_terminal() {
                    return Ok((snapshot.view, streamed));
                }
            }
            AgentSubscriptionFrame::Ended { run_view, .. } => {
                finish_streaming_region(
                    render,
                    decorated,
                    &mut streaming_region_active,
                    theme,
                    started.elapsed(),
                )?;
                return Ok((run_view, streamed));
            }
            AgentSubscriptionFrame::Subscribed { .. } => unreachable!(),
        }
    }
}

pub(crate) fn render_progress(
    progress: AgentProgress,
    render: bool,
    full: bool,
    streamed: &mut String,
    _streaming_region_active: &mut bool,
    _started: Instant,
    _theme: &TerminalTheme,
) -> Result<()> {
    match progress {
        AgentProgress::ModelOutputDelta { content, .. } => {
            let delta = content.as_text();
            streamed.push_str(delta);
            if render && full {
                let _ = send_tty(TtyMessage::ModelDelta(delta.to_owned()));
            }
        }
        AgentProgress::OperationStatus { operation, status } if render => {
            finish_streaming_region(
                render,
                full,
                _streaming_region_active,
                _theme,
                _started.elapsed(),
            )?;
            match status {
                AgentProgressStatus::Running if full => {}
                AgentProgressStatus::Running => {
                    begin_external_block();
                    eprintln!("operation {} running", operation.ordinal)
                }
                AgentProgressStatus::Terminal(_) if full => {}
                AgentProgressStatus::Terminal(status) => {
                    begin_external_block();
                    eprintln!("operation {} {status:?}", operation.ordinal)
                }
            }
        }
        AgentProgress::OperationStatus { .. } => {}
    }
    Ok(())
}

pub(crate) fn finish_streaming_region(
    render: bool,
    full: bool,
    active: &mut bool,
    theme: &TerminalTheme,
    elapsed: Duration,
) -> Result<()> {
    if render && full && *active {
        println!();
        println!("{}", answer_footer(theme, elapsed));
        println!();
        std::io::stdout().flush()?;
        *active = false;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn render_event(
    client: &AgentClient,
    event: &AgentEvent,
    render: bool,
    full: bool,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    operation_started_at_ms: &mut BTreeMap<agl_core::agent::AgentOperationKey, i64>,
    theme: &TerminalTheme,
    model_generation_details: bool,
) {
    if !render {
        return;
    }
    match &event.data {
        AgentEventData::OperationCreated {
            key: _,
            kind: AgentOperationKind::Tool,
            tool_id: Some(tool_id),
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity(format!("Running {tool_id}")));
            if !full
                && let AgentEventData::OperationCreated {
                    key,
                    tool_id: Some(tool_id),
                    ..
                } = &event.data
            {
                eprintln!("tool {} ({})", tool_id, key.ordinal);
            }
        }
        AgentEventData::OperationCreated {
            kind: AgentOperationKind::ModelGeneration,
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity("Generating".to_owned()));
        }
        AgentEventData::OperationCreated {
            kind: AgentOperationKind::Compaction,
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity("Compacting".to_owned()));
        }
        AgentEventData::OperationCreated { .. } if full => {
            // Details are emitted once, when the operation reaches a terminal state.
        }
        AgentEventData::OperationStarted { key, .. } => {
            operation_started_at_ms
                .entry(key.clone())
                .or_insert(event.committed_at_ms);
            // OperationCreated establishes the activity (Tool or model). Do
            // not blindly turn a running Tool into Generating here.
        }
        AgentEventData::OperationCompleted { key, status, .. } if full => {
            let tool_elapsed = operation_started_at_ms
                .remove(key)
                .map(|started| elapsed_between_ms(started, event.committed_at_ms));
            render_operation_detail(
                client,
                key,
                None,
                None,
                tool_output,
                tool_frame,
                tool_elapsed,
                theme,
                model_generation_details,
            )
            .await;
            let _ = status;
        }
        AgentEventData::OperationRetryScheduled {
            key,
            delivery_attempt,
            ..
        } => {
            let _ = send_tty(TtyMessage::Activity("Retrying".to_owned()));
            let line = format!(
                "operation {} retry scheduled after attempt {}",
                key.ordinal, delivery_attempt
            );
            begin_external_block();
            if full {
                println!("{line}");
            } else {
                eprintln!("{line}");
            }
        }
        AgentEventData::OperationOutcomeUnknown { key, .. } => {
            begin_external_block();
            if full {
                println!("operation {} outcome unknown", key.ordinal)
            } else {
                eprintln!("operation {} outcome unknown", key.ordinal)
            }
        }
        AgentEventData::RunStatusChanged { to, .. } if to.is_terminal() => {
            let _ = to;
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn render_operation_detail(
    client: &AgentClient,
    key: &agl_core::agent::AgentOperationKey,
    kind: Option<AgentOperationKind>,
    delivery: Option<agl_core::agent::DeliveryClass>,
    tool_output: agl_core::agent::ToolOutputPresentation,
    tool_frame: bool,
    tool_elapsed: Option<Duration>,
    theme: &TerminalTheme,
    model_generation_details: bool,
) {
    match client.operation_view(key.clone()).await {
        Ok(operation) => match operation_request_decoration(&operation.request) {
            Ok(request) => {
                begin_external_block();
                if let agl_core::agent::AgentOperationRequest::Tool(tool) = &operation.request {
                    if tool_frame {
                        println!();
                        println!("{}", answer_rule(theme));
                        println!();
                    }
                    let state = format!("{:?}", operation.state);
                    println!(
                        "{} {}  {}  {}",
                        theme.paint("tool", "TOOL"),
                        theme.paint("tool", tool.tool_id.as_str()),
                        theme.paint("ordinal", &format!("#{}", key.ordinal)),
                        theme.paint(operation_status_role(&state), &state),
                    );
                    if tool.tool_id.as_str() == "agentlibre.execution:command.exec"
                        && let Some(argv) = tool.input.get("argv")
                    {
                        println!(
                            "  {} {}",
                            theme.paint("muted", "command"),
                            json_to_terminal(argv, theme)
                        );
                    }
                    let input = json_to_terminal(&tool.input, theme);
                    println!(
                        "  {}\n{}",
                        theme.paint("field", "INPUT"),
                        preview_tool_output(&input, tool_output, key.ordinal.get(), "input", theme)
                    );
                } else {
                    let kind = kind.unwrap_or_else(|| operation_request_kind(&operation.request));
                    let kind = format!("{kind:?}");
                    let state = format!("{:?}", operation.state);
                    let compaction_required = operation.failure.as_ref().is_some_and(|failure| {
                        matches!(
                            failure.kind,
                            agl_core::agent::AgentOperationFailureKind::CompactionRequired(_)
                        )
                    });
                    if compaction_required {
                        println!(
                            "{} {}  {}",
                            theme.paint("operation", "MODEL GENERATION"),
                            theme.paint("ordinal", &format!("#{}", key.ordinal)),
                            theme.paint("muted", "COMPACTION REQUIRED"),
                        );
                        return;
                    }
                    if !model_generation_details && kind == "ModelGeneration" {
                        println!(
                            "{} {}  {}",
                            theme.paint("operation", "MODEL GENERATION"),
                            theme.paint("ordinal", &format!("#{}", key.ordinal)),
                            theme.paint(operation_status_role(&state), &state),
                        );
                        return;
                    }
                    let label = match kind.as_str() {
                        "ModelGeneration" => "MODEL GENERATION",
                        "Compaction" => "COMPACTION",
                        _ => "OPERATION",
                    };
                    println!(
                        "{} {}  {}",
                        theme.paint("operation", label),
                        theme.paint("ordinal", &format!("#{}", key.ordinal)),
                        theme.paint(operation_status_role(&state), &state),
                    );
                    let delivery = delivery.unwrap_or(operation.delivery);
                    println!(
                        "  {} {}  {} {}",
                        theme.paint("muted", "delivery"),
                        theme.paint("operation", &format!("{delivery:?}")),
                        theme.paint("muted", "attempt"),
                        theme.paint("status_pending", &operation.delivery_attempt.to_string()),
                    );
                    let request = serde_json::from_str(&request)
                        .map(|value| json_to_terminal(&value, theme))
                        .unwrap_or(request);
                    println!("  {}\n{}", theme.paint("field", "REQUEST"), request);
                }
                if let Some(result) = operation.result.as_ref() {
                    let variant = match result {
                        agl_core::agent::AgentOperationResult::ModelGeneration(_) => {
                            "model_generation"
                        }
                        agl_core::agent::AgentOperationResult::Compaction(_) => "compaction",
                        agl_core::agent::AgentOperationResult::Tool(_) => "tool",
                    };
                    if let agl_core::agent::AgentOperationResult::Tool(result) = result {
                        println!(
                            "  {}\n{}",
                            theme.paint("field", "RESULT"),
                            frame_tool_output(
                                &preview_tool_output(
                                    &format_tool_content_for(
                                        match &operation.request {
                                            agl_core::agent::AgentOperationRequest::Tool(tool) =>
                                                Some(tool.tool_id.as_str()),
                                            _ => None,
                                        },
                                        result.content.as_text(),
                                        match &operation.request {
                                            agl_core::agent::AgentOperationRequest::Tool(tool) =>
                                                Some(&tool.input),
                                            _ => None,
                                        },
                                        theme,
                                    ),
                                    tool_output,
                                    key.ordinal.get(),
                                    "result",
                                    theme,
                                ),
                                theme
                            )
                        );
                    } else {
                        println!(
                            "  {} {}",
                            theme.paint("field", "RESULT"),
                            theme.paint("json_string", variant)
                        );
                    }
                }
                if let Some(failure) = operation.failure.as_ref() {
                    let bytes = serde_json::to_vec(failure)
                        .map(|value| value.len())
                        .unwrap_or(0);
                    let failure = serde_json::to_value(&failure.kind)
                        .map(|value| json_to_terminal(&value, theme))
                        .unwrap_or_else(|_| safe_terminal_text(&format!("{:?}", failure.kind)));
                    println!("  {}", theme.paint("status_failure", "FAILURE"));
                    println!("{failure}");
                    println!(
                        "  {}",
                        theme.paint("muted", &format!("diagnostic bytes: {bytes}"))
                    );
                }
                if tool_frame
                    && matches!(
                        &operation.request,
                        agl_core::agent::AgentOperationRequest::Tool(_)
                    )
                {
                    println!("{}", answer_footer(theme, tool_elapsed.unwrap_or_default()));
                    println!();
                }
            }
            Err(error) => eprintln!("operation {} decoration diagnostic: {error}", key.ordinal),
        },
        Err(error) => eprintln!("operation {} decoration diagnostic: {error}", key.ordinal),
    }
}

pub(crate) fn operation_request_decoration(
    request: &agl_core::agent::AgentOperationRequest,
) -> Result<String, serde_json::Error> {
    use agl_core::agent::AgentOperationRequest;
    let value = match request {
        AgentOperationRequest::ModelGeneration(request) => serde_json::json!({
            "type": "model_generation",
            "context_messages": request.context.len(),
            "max_output_tokens": request.max_output_tokens,
        }),
        AgentOperationRequest::Compaction(request) => serde_json::json!({
            "type": "compaction",
            "context_messages": request.context.len(),
            "before": request.before,
            "correction_of": request.correction_of,
        }),
        AgentOperationRequest::Tool(request) => serde_json::json!({
            "type": "tool",
            "tool_id": request.tool_id,
            "input": request.input,
        }),
    };
    serde_json::to_string(&value)
}

pub(crate) fn operation_request_kind(
    request: &agl_core::agent::AgentOperationRequest,
) -> AgentOperationKind {
    match request {
        agl_core::agent::AgentOperationRequest::ModelGeneration(_) => {
            AgentOperationKind::ModelGeneration
        }
        agl_core::agent::AgentOperationRequest::Compaction(_) => AgentOperationKind::Compaction,
        agl_core::agent::AgentOperationRequest::Tool(_) => AgentOperationKind::Tool,
    }
}

pub(crate) fn event_ends_run(event: &AgentEvent) -> bool {
    matches!(
        event.data,
        AgentEventData::RunStatusChanged { to, .. } if to.is_terminal()
    )
}

pub(crate) fn terminal_summary(view: &agl_core::agent::AgentRunView) -> String {
    let mut summary = format!("AgentRun ended with {:?}", view.status);
    if let Some(failure) = &view.failure {
        match failure {
            agl_core::agent::AgentRunFailureView::Run { kind } => {
                summary.push_str(&format!(": Run failed with {}", enum_name(kind)));
            }
            agl_core::agent::AgentRunFailureView::Operation { kind, tool_id, .. } => {
                let kind = match kind {
                    agl_core::agent::AgentOperationFailureKind::ContextExhausted(failure) => {
                        let capacity = &failure.capacity;
                        let mut diagnostic = format!(
                            "context_exhausted (prompt_tokens={}, reserved_output_tokens={}, context_capacity_tokens={}, trigger_threshold_tokens={})",
                            capacity.prompt_tokens,
                            capacity.reserved_output_tokens,
                            capacity.context_capacity_tokens,
                            capacity.trigger_threshold_tokens,
                        );
                        if let Some(compaction) = &failure.compaction {
                            diagnostic.push_str(&format!(
                                " compaction={}",
                                serde_json::to_string(compaction)
                                    .expect("typed compaction diagnostic")
                            ));
                        }
                        diagnostic
                    }
                    _ => enum_name(kind),
                };
                match tool_id {
                    Some(tool_id) => {
                        summary.push_str(&format!(": Tool {tool_id} failed with {kind}"));
                    }
                    None => summary.push_str(&format!(": operation failed with {kind}")),
                }
            }
        }
    }
    summary
}

pub(crate) fn enum_name<T: serde::Serialize + std::fmt::Debug>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{value:?}"))
}
