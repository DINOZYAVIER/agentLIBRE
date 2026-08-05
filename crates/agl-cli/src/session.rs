use std::path::Path;

use agl_client::{AgentLibreClient, PresentationSubscriptionEvent};
use agl_protocol::{
    ProtocolToolMode, RunBudgetRequest, RunSubmitRequest, SessionCancelActiveRequest,
    SessionFinishReason, SessionFinishRequest, SessionListRequest, SessionOpenRequest,
    SessionPresentationSubscribeRequest, SessionStatusRequest, SessionTranscriptRequest,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result};

use crate::args::{SessionCommand, SessionOptions, ToolAccessMode};

pub(crate) fn run_session(
    options: SessionOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let socket = options
        .socket_path
        .unwrap_or_else(|| agl_daemon::default_socket_path(&runtime.paths));
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build session command runtime")?;
    async_runtime.block_on(run_session_async(options.command, runtime, &socket))
}

async fn run_session_async(
    command: SessionCommand,
    runtime: &AgentLibreRuntimeConfig,
    socket: &Path,
) -> Result<()> {
    let client = crate::runtime::connect_daemon(socket)
        .await
        .context("failed to connect to the agent daemon")?;
    match command {
        SessionCommand::New(options) => {
            let workspace_root = options
                .workspace_root
                .map(Ok)
                .unwrap_or_else(|| runtime.resolve_workspace_root(None))?;
            let opened = client
                .open_session(SessionOpenRequest {
                    session_id: None,
                    new_session: true,
                    workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
                    function_ref: options.function_ref,
                    skills: options.skills,
                    tool_mode: protocol_tool_mode(options.tool_mode),
                })
                .await?;
            print_value(options.json, &opened, || println!("{}", opened.session_id))
        }
        SessionCommand::List { json } => {
            let listed = client.list_sessions(SessionListRequest::default()).await?;
            print_value(json, &listed, || {
                for session in &listed.sessions {
                    println!("{}\t{:?}", session.session_id, session.status);
                }
            })
        }
        SessionCommand::Show(options) => {
            let status = client
                .session_status(SessionStatusRequest {
                    session_id: options.session_id.clone(),
                })
                .await?;
            let transcript = client
                .read_transcript(SessionTranscriptRequest {
                    session_id: options.session_id,
                    include_content: options.include_content,
                })
                .await?;
            let value = serde_json::json!({"status": status, "transcript": transcript});
            print_value(options.json, &value, || {
                println!("session={} status={:?}", status.session_id, status.status);
                for event in &transcript.events {
                    println!("{event:?}");
                }
            })
        }
        SessionCommand::Resume(options) => {
            let opened = client
                .open_session(SessionOpenRequest {
                    session_id: Some(options.session_id),
                    new_session: false,
                    workspace_root: None,
                    function_ref: None,
                    skills: Vec::new(),
                    tool_mode: ProtocolToolMode::ReadOnly,
                })
                .await?;
            print_value(options.json, &opened, || println!("{}", opened.session_id))
        }
        SessionCommand::Submit(options) => {
            let accepted = client
                .submit_prompt(RunSubmitRequest {
                    session_id: options.session_id,
                    content: agl_content::Content::text(options.prompt)?,
                    client_submission_id: format!(
                        "cli-session-submit-{}",
                        agl_ids::RequestId::generate()
                    ),
                    budget: RunBudgetRequest::default(),
                })
                .await?;
            print_value(options.json, &accepted, || println!("{}", accepted.run_id))
        }
        SessionCommand::Follow(options) => follow(&client, options.session_id, options.json).await,
        SessionCommand::Cancel(options) => {
            let cancelled = client
                .cancel_active_session_work(SessionCancelActiveRequest {
                    session_id: options.session_id,
                })
                .await?;
            print_value(options.json, &cancelled, || {
                println!(
                    "session={} cancelled_runs={} terminated_terminals={} terminated_executions={}",
                    cancelled.session_id,
                    cancelled.cancelled_runs,
                    cancelled.terminated_terminals,
                    cancelled.terminated_executions
                )
            })
        }
        SessionCommand::Finish(options) => {
            let finished = client
                .finish_session(SessionFinishRequest {
                    session_id: options.session_id,
                    reason: SessionFinishReason::ExitCommand,
                })
                .await?;
            print_value(options.json, &finished, || {
                println!("{}", finished.session_id)
            })
        }
    }
}

async fn follow(
    client: &AgentLibreClient,
    session_id: agl_ids::SessionId,
    json: bool,
) -> Result<()> {
    let mut subscription = client
        .subscribe_presentation(SessionPresentationSubscribeRequest { session_id })
        .await?;
    while let Some(event) = subscription.next().await? {
        match event {
            PresentationSubscriptionEvent::SnapshotReplaced { event_id, snapshot } => {
                if json {
                    crate::print_json(&serde_json::json!({
                        "kind": "snapshot_replaced",
                        "event_id": event_id,
                        "snapshot": snapshot,
                    }))?;
                } else {
                    println!("snapshot_replaced event_id={event_id}");
                }
            }
            PresentationSubscriptionEvent::Event(event) => {
                if json {
                    crate::print_json(&event)?;
                } else {
                    println!("{:?}", event.event);
                }
            }
            PresentationSubscriptionEvent::Finished(event) => {
                if json {
                    crate::print_json(&event)?;
                } else {
                    println!("finished reason={:?}", event.reason);
                }
                break;
            }
        }
    }
    Ok(())
}

fn protocol_tool_mode(mode: ToolAccessMode) -> ProtocolToolMode {
    match mode {
        ToolAccessMode::ReadOnly => ProtocolToolMode::ReadOnly,
        ToolAccessMode::Write => ProtocolToolMode::Write,
        ToolAccessMode::Execute => ProtocolToolMode::Execute,
        ToolAccessMode::Approve => ProtocolToolMode::Approve,
        ToolAccessMode::Admin => ProtocolToolMode::Admin,
    }
}

fn print_value<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce()) -> Result<()> {
    crate::print_json_or(json, value, text)
}
