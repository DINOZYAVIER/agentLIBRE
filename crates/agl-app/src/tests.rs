use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use agl_capabilities::ToolAccessMode;
use agl_content::Content;
use agl_ids::{
    AttemptId, DaemonInstanceId, ExecutionId, MessageId, RunId, SessionId, TerminalSessionId,
    TurnId,
};
use agl_process::{ExecutionProfile, ExecutionState, TerminalSize};

use super::*;

#[test]
fn command_catalog_has_the_selected_unique_surface_and_busy_availability() {
    let catalog = shared_command_catalog(&CommandContext {
        session_id: Some(SessionId::generate()),
        session_active: true,
        active_or_queued_turns: 1,
        active_executions: 1,
        host_shell_available: false,
        operation_mode: ToolAccessMode::Execute,
    });
    catalog.validate().unwrap();
    let ids = catalog
        .descriptors
        .iter()
        .map(|descriptor| descriptor.id.to_string())
        .collect::<BTreeSet<_>>();
    let names = catalog
        .descriptors
        .iter()
        .map(|descriptor| descriptor.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), catalog.descriptors.len());
    assert_eq!(names.len(), catalog.descriptors.len());
    assert_eq!(
        names,
        [
            "attach",
            "clear",
            "disconnect",
            "exit",
            "help",
            "kill",
            "mode",
            "model",
            "new",
            "processes",
            "reload",
            "resume",
            "skills",
            "status",
            "workspace",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    let model = catalog
        .descriptors
        .iter()
        .find(|descriptor| descriptor.id.as_str() == "model.select")
        .unwrap();
    assert!(matches!(
        model.availability,
        CommandAvailability::Disabled { ref reason_code, .. } if reason_code == "session_busy"
    ));
    for id in ["session.new", "session.resume", "session.exit"] {
        let descriptor = catalog
            .descriptors
            .iter()
            .find(|descriptor| descriptor.id.as_str() == id)
            .unwrap();
        assert_eq!(descriptor.availability, CommandAvailability::Enabled);
    }
    for forbidden in ["cd", "pwd", "session", "quit", "finish"] {
        assert!(!names.contains(forbidden));
    }
    assert!(catalog.descriptors.iter().all(|descriptor| {
        !matches!(
            descriptor.action_kind,
            ApplicationActionKind::TerminalList | ApplicationActionKind::TerminalPromote
        )
    }));
}

#[test]
fn prompt_queue_is_fifo_bounded_and_submission_idempotent() {
    let session_id = SessionId::generate();
    let mut queue = PromptQueue::default();
    let first_submission = prompt(&session_id, "first");
    let first = queue
        .admit(&first_submission, RunId::generate(), TurnId::generate())
        .unwrap();
    let replay = queue
        .admit(&first_submission, RunId::generate(), TurnId::generate())
        .unwrap();
    assert_eq!(replay.run_id, first.run_id);
    assert!(!first.queued);

    let second = queue
        .admit(
            &prompt(&session_id, "second"),
            RunId::generate(),
            TurnId::generate(),
        )
        .unwrap();
    assert!(second.queued);
    assert_eq!(
        queue.finish(&session_id, &first.run_id).unwrap().run_id,
        second.run_id
    );
}

#[test]
fn human_terminal_ensure_is_strict_bounded_and_redacts_secret_references() {
    let mut request = human_terminal_ensure(ExecutionProfile::Workspace);
    request.validate().unwrap();

    request.host_startup = HostStartupPolicy::SourceUserRc;
    assert_eq!(
        request.validate().unwrap_err().code,
        ApplicationErrorCode::InvalidArguments
    );

    request.profile = ExecutionProfile::Host;
    request.agl_env.values = BTreeMap::from([("TOKEN".to_owned(), "not-secret".to_owned())]);
    request.agl_env.secret_refs = vec![SecretEnvironmentReference {
        name: "TOKEN".to_owned(),
        reference_id: "vault:private-token".to_owned(),
    }];
    assert!(request.validate().is_err());
    assert!(!format!("{request:?}").contains("vault:private-token"));
    assert!(!format!("{request:?}").contains("not-secret"));

    request.agl_env.secret_refs.clear();
    request.agl_env.values = BTreeMap::from([("PATH".to_owned(), "/tmp/bin".to_owned())]);
    assert!(request.validate().is_err());
}

#[test]
fn terminal_projection_rejects_cross_authority_host_and_inconsistent_promotion() {
    let session_id = SessionId::generate();
    let mut terminal = terminal(&session_id);
    terminal.validate_for_session(&session_id).unwrap();

    terminal.profile = ExecutionProfile::Host;
    terminal.owner = TerminalOwnerView::MainAgent {
        session_id: session_id.clone(),
    };
    assert!(terminal.validate().is_err());

    terminal.profile = ExecutionProfile::Workspace;
    terminal.owner = TerminalOwnerView::SessionPromoted {
        session_id,
        previous_owner_run_id: RunId::generate(),
    };
    terminal.promoted = false;
    assert!(terminal.validate().is_err());
}

#[test]
fn terminal_picker_actions_are_typed_and_workspace_confirmation_is_explicit() {
    let session_id = SessionId::generate();
    let terminal_id = TerminalSessionId::generate();
    let list = ApplicationActionRequest {
        session_id: Some(session_id.clone()),
        client_submission_id: "list-terminals".to_owned(),
        action: ApplicationAction::TerminalList {
            include_finished: true,
        },
    };
    list.validate().unwrap();
    let promote = ApplicationActionRequest {
        session_id: Some(session_id.clone()),
        client_submission_id: "promote-terminal".to_owned(),
        action: ApplicationAction::TerminalPromote {
            terminal_id: terminal_id.clone(),
        },
    };
    promote.validate().unwrap();
    let workspace = ApplicationActionRequest {
        session_id: Some(session_id),
        client_submission_id: "workspace-change".to_owned(),
        action: ApplicationAction::WorkspaceSet {
            path: "/next-workspace".to_owned(),
            confirm_terminate_terminals: true,
        },
    };
    workspace.validate().unwrap();

    let encoded = serde_json::to_string(&(list, promote, workspace)).unwrap();
    assert!(encoded.contains(terminal_id.as_str()));
    assert!(encoded.contains("confirm_terminate_terminals"));
}

#[test]
fn human_history_commands_are_bounded_and_private_in_debug_output() {
    let command = HumanShellHistoryCommand::new(
        TerminalSessionId::generate(),
        7,
        "/workspace",
        "printf private-value",
    )
    .unwrap();
    assert_eq!(command.command(), "printf private-value");
    assert!(!format!("{command:?}").contains("private-value"));
    assert_eq!(
        HumanShellHistoryPolicy::selected().maximum_entries,
        HUMAN_SHELL_HISTORY_MAX_ENTRIES
    );
}

#[test]
fn application_error_truncates_multibyte_text_on_a_utf8_boundary() {
    let error = ApplicationError::new(ApplicationErrorCode::Internal, "я".repeat(8 * 1024));
    assert!(error.message.len() <= 8 * 1024);
    assert!(std::str::from_utf8(error.message.as_bytes()).is_ok());
}

#[tokio::test]
async fn presentation_snapshot_and_live_registration_are_revision_contiguous() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let snapshot = snapshot(&daemon_instance_id, &session_id);
    let service = ApplicationService::new(
        daemon_instance_id.clone(),
        Arc::new(FakeBackend {
            snapshot: snapshot.clone(),
            older_page_cursor: None,
            exit_on_invoke: false,
        }),
    );
    let mut subscription = service
        .subscribe(PresentationSubscribe {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(subscription.snapshot.cursor.revision, 0);
    service
        .publish(
            &session_id,
            SessionPresentationEvent::Notice {
                severity: Severity::Info,
                code: "ready".to_owned(),
                message: "ready".to_owned(),
            },
        )
        .unwrap();
    let event = subscription.next().await.unwrap();
    assert_eq!(event.cursor.daemon_instance_id, daemon_instance_id);
    assert_eq!(event.cursor.revision, 1);
}

#[tokio::test]
async fn presentation_page_cursor_is_preserved_on_initial_and_replacement_snapshots() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let older_page_cursor = "p1.session-scope.10".to_owned();
    let service = ApplicationService::new(
        daemon_instance_id.clone(),
        Arc::new(FakeBackend {
            snapshot: snapshot(&daemon_instance_id, &session_id),
            older_page_cursor: Some(older_page_cursor.clone()),
            exit_on_invoke: false,
        }),
    );
    let mut subscription = service
        .subscribe(PresentationSubscribe {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        subscription.older_page_cursor.as_deref(),
        Some(older_page_cursor.as_str())
    );

    service.refresh(&session_id).await.unwrap();
    let replacement = subscription.next().await.unwrap();
    assert!(matches!(
        replacement.event,
        SessionPresentationEvent::SnapshotReplaced {
            older_page_cursor: Some(ref cursor),
            ..
        } if cursor == &older_page_cursor
    ));

    let error = service
        .snapshot_page(&session_id, Some(String::new()))
        .await
        .unwrap_err();
    assert_eq!(error.code, ApplicationErrorCode::InvalidArguments);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborted_application_calls_keep_the_blocking_bridge_bounded() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let backend = Arc::new(BlockingSnapshotBackend {
        snapshot: snapshot(&daemon_instance_id, &session_id),
        entered: AtomicUsize::new(0),
        cancelled: AtomicUsize::new(0),
        release: AtomicBool::new(false),
    });
    let service = ApplicationService::new(daemon_instance_id, backend.clone());
    let mut calls = Vec::new();
    for _ in 0..32 {
        let service = service.clone();
        let session_id = session_id.clone();
        calls.push(tokio::spawn(async move {
            service.snapshot_page(&session_id, None).await
        }));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.entered.load(Ordering::Acquire) != 32 {
        assert!(
            Instant::now() < deadline,
            "all admitted blocking calls must start"
        );
        tokio::task::yield_now().await;
    }
    for call in &calls {
        call.abort();
    }
    for call in calls {
        let _ = call.await;
    }
    assert_eq!(service.available_blocking_permits(), 0);
    let error = service.snapshot_page(&session_id, None).await.unwrap_err();
    assert_eq!(error.code, ApplicationErrorCode::InputBackpressure);

    backend.release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while service.available_blocking_permits() != 32 {
        assert!(
            Instant::now() < deadline,
            "detached blocking calls must release their charged permits"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(
        backend.cancelled.load(Ordering::Acquire),
        32,
        "aborting each async awaiter must cancel its owner-call context"
    );
}

#[tokio::test]
async fn a_second_subscriber_cannot_install_different_state_at_the_same_revision() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let first = snapshot(&daemon_instance_id, &session_id);
    let mut second = first.clone();
    second.header.title = Some("state after prompt admission".to_owned());
    let service = ApplicationService::new(
        daemon_instance_id,
        Arc::new(SequencedSnapshotBackend {
            snapshots: Mutex::new(VecDeque::from([first, second])),
            admission: None,
        }),
    );

    let mut first_subscriber = service
        .subscribe(PresentationSubscribe {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let second_subscriber = service
        .subscribe(PresentationSubscribe { session_id })
        .await
        .unwrap();
    let replacement = first_subscriber.next().await.unwrap();

    assert_eq!(replacement.cursor, second_subscriber.snapshot.cursor);
    assert_eq!(replacement.cursor.revision, 1);
    assert!(matches!(
        replacement.event,
        SessionPresentationEvent::SnapshotReplaced { snapshot, .. }
            if snapshot.as_ref() == &second_subscriber.snapshot
    ));
}

#[tokio::test]
async fn prompt_admission_publishes_snapshot_and_typed_queue_transition() {
    for queued in [false, true] {
        let daemon_instance_id = DaemonInstanceId::generate();
        let session_id = SessionId::generate();
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        let first = snapshot(&daemon_instance_id, &session_id);
        let mut admitted = first.clone();
        if queued {
            admitted.queued_prompts.push(QueuedPromptView {
                run_id: run_id.clone(),
                ordinal: 2,
            });
            admitted.header.queued_prompt_count = 1;
        } else {
            admitted.active_run = Some(ActiveRunView {
                run_id: run_id.clone(),
                turn_id: Some(turn_id.clone()),
                state: "running".to_owned(),
            });
            admitted.header.active_run_count = 1;
        }
        admitted.command_context.active_or_queued_turns = 1;
        let service = ApplicationService::new(
            daemon_instance_id,
            Arc::new(SequencedSnapshotBackend {
                snapshots: Mutex::new(VecDeque::from([first, admitted])),
                admission: Some(PromptAdmission {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id,
                    ordinal: if queued { 2 } else { 1 },
                    queued,
                    state: if queued {
                        PromptAdmissionState::Queued
                    } else {
                        PromptAdmissionState::Running
                    },
                    replayed: false,
                }),
            }),
        );
        let mut subscription = service
            .subscribe(PresentationSubscribe {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();

        service
            .submit_prompt(PromptSubmission {
                session_id,
                client_submission_id: format!("prompt-{queued}"),
                content: Content::text("hello").unwrap(),
                budget: PromptBudget::default(),
            })
            .await
            .unwrap();

        assert!(matches!(
            subscription.next().await.unwrap().event,
            SessionPresentationEvent::SnapshotReplaced { .. }
        ));
        let transition = subscription.next().await.unwrap();
        if queued {
            assert!(matches!(
                transition.event,
                SessionPresentationEvent::PromptQueued { prompt }
                    if prompt.run_id == run_id && prompt.ordinal == 2
            ));
        } else {
            assert!(matches!(
                transition.event,
                SessionPresentationEvent::PromptActivated { run_id: activated }
                    if activated == run_id
            ));
        }
    }
}

#[test]
fn presentation_snapshot_rejects_decoded_content_over_eight_mib() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let mut snapshot = snapshot(&daemon_instance_id, &session_id);
    for _ in 0..9 {
        snapshot.items.push(SessionPresentationItem::UserMessage {
            message_id: agl_ids::MessageId::generate(),
            content: agl_content::Content::text("x".repeat(agl_content::MAX_TEXT_PART_BYTES))
                .unwrap(),
        });
    }

    let error = snapshot.validate().unwrap_err();
    assert_eq!(error.code, ApplicationErrorCode::InvalidArguments);
    assert!(error.message.contains("8 MiB"));
}

#[tokio::test]
async fn session_exit_publishes_a_terminal_boundary_to_peer_subscribers() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let service = ApplicationService::new(
        daemon_instance_id.clone(),
        Arc::new(FakeBackend {
            snapshot: snapshot(&daemon_instance_id, &session_id),
            older_page_cursor: None,
            exit_on_invoke: true,
        }),
    );
    let mut subscription = service
        .subscribe(PresentationSubscribe {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();

    let result = service
        .invoke(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: "finish-peer-projection".to_owned(),
            action: ApplicationAction::SessionExit {
                confirm_active: true,
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        result,
        ApplicationActionResult::SessionExited { .. }
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::SnapshotReplaced { .. }
    ));
    let finished = subscription.next().await.unwrap();
    assert_eq!(finished.cursor.revision, 2);
    assert!(matches!(
        finished.event,
        SessionPresentationEvent::SessionFinished
    ));
}

#[tokio::test]
async fn chat_presentation_proxy_is_nonblocking_and_reconciles_by_message_id() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let service = ApplicationService::new(
        daemon_instance_id,
        Arc::new(FakeBackend {
            snapshot: snapshot(&DaemonInstanceId::generate(), &session_id),
            older_page_cursor: None,
            exit_on_invoke: false,
        }),
    );
    let mut subscription = service
        .subscribe(PresentationSubscribe {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let proxy = TurnPresentationProxy::new();
    assert_eq!(
        agl_chat::TurnPresentationSink::try_publish(
            &proxy,
            agl_chat::TurnPresentationEvent::ModelAttemptStarted {
                session_id: session_id.clone(),
                run_id: RunId::generate(),
                turn_id: TurnId::generate(),
                attempt_id: AttemptId::generate(),
                provisional_message_id: MessageId::generate(),
            },
        ),
        agl_chat::PresentationDelivery::Closed
    );
    proxy.connect(service.clone()).unwrap();
    assert!(proxy.is_connected());
    assert_eq!(
        proxy.connect(service.clone()).unwrap_err().code,
        ApplicationErrorCode::InvalidArguments
    );

    let run_id = RunId::generate();
    let turn_id = TurnId::generate();
    let attempt_id = AttemptId::generate();
    let message_id = MessageId::generate();
    assert_eq!(
        agl_chat::TurnPresentationSink::try_publish(
            &proxy,
            agl_chat::TurnPresentationEvent::ModelAttemptStarted {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                attempt_id: attempt_id.clone(),
                provisional_message_id: message_id.clone(),
            },
        ),
        agl_chat::PresentationDelivery::Delivered
    );
    assert_eq!(
        agl_chat::TurnPresentationSink::try_publish(
            &proxy,
            agl_chat::TurnPresentationEvent::AssistantTextDelta {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                attempt_id: attempt_id.clone(),
                provisional_message_id: message_id.clone(),
                sequence: 1,
                text: "привет".to_owned(),
            },
        ),
        agl_chat::PresentationDelivery::Delivered
    );
    let during_stream = service.snapshot(&session_id).await.unwrap();
    assert!(during_stream.items.iter().any(|item| {
        matches!(
            item,
            SessionPresentationItem::AssistantMessage {
                message_id: item_message_id,
                content,
                state: AssistantItemState::Streaming,
            } if item_message_id == &message_id
                && content.text_only().as_deref() == Some("привет")
        )
    }));
    let retry_attempt_id = AttemptId::generate();
    assert_eq!(
        agl_chat::TurnPresentationSink::try_publish(
            &proxy,
            agl_chat::TurnPresentationEvent::ModelAttemptStarted {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                attempt_id: retry_attempt_id.clone(),
                provisional_message_id: message_id.clone(),
            },
        ),
        agl_chat::PresentationDelivery::Delivered
    );
    assert_eq!(
        agl_chat::TurnPresentationSink::try_publish(
            &proxy,
            agl_chat::TurnPresentationEvent::AssistantTextDelta {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                attempt_id: retry_attempt_id.clone(),
                provisional_message_id: message_id.clone(),
                sequence: 1,
                text: "снова".to_owned(),
            },
        ),
        agl_chat::PresentationDelivery::Delivered
    );
    let during_retry = service.snapshot(&session_id).await.unwrap();
    assert!(during_retry.items.iter().any(|item| {
        matches!(
            item,
            SessionPresentationItem::AssistantMessage {
                message_id: item_message_id,
                content,
                state: AssistantItemState::Streaming,
            } if item_message_id == &message_id
                && content.text_only().as_deref() == Some("снова")
        )
    }));
    assert_eq!(
        agl_chat::TurnPresentationSink::try_publish(
            &proxy,
            agl_chat::TurnPresentationEvent::AssistantMessageFinal {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_id,
                attempt_id: Some(retry_attempt_id),
                message_id: message_id.clone(),
                content: Content::text("снова").unwrap(),
            },
        ),
        agl_chat::PresentationDelivery::Delivered
    );

    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::PromptActivated { run_id: ref activated }
            if activated == &run_id
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::ItemRemoved { ref item_key }
            if item_key == message_id.as_str()
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::Notice { ref code, .. } if code == "model_attempt_started"
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::AssistantTextDelta {
            ref provisional_message_id,
            sequence: 1,
            ref text,
            ..
        } if provisional_message_id == &message_id && text == "привет"
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::SnapshotReplaced { .. }
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::PromptActivated { run_id: ref activated }
            if activated == &run_id
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::ItemRemoved { ref item_key }
            if item_key == message_id.as_str()
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::Notice { ref code, .. } if code == "model_attempt_started"
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::AssistantTextDelta {
            ref provisional_message_id,
            sequence: 1,
            ref text,
            ..
        } if provisional_message_id == &message_id && text == "снова"
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::SnapshotReplaced { .. }
    ));
    assert!(matches!(
        subscription.next().await.unwrap().event,
        SessionPresentationEvent::ItemUpsert {
            item: SessionPresentationItem::AssistantMessage {
                message_id: ref final_message_id,
                state: AssistantItemState::Final,
                ..
            }
        } if final_message_id == &message_id
    ));
}

#[tokio::test]
async fn terminal_events_update_metadata_without_a_raw_output_variant() {
    let daemon_instance_id = DaemonInstanceId::generate();
    let session_id = SessionId::generate();
    let service = ApplicationService::new(
        daemon_instance_id.clone(),
        Arc::new(FakeBackend {
            snapshot: snapshot(&daemon_instance_id, &session_id),
            older_page_cursor: None,
            exit_on_invoke: false,
        }),
    );
    service.snapshot(&session_id).await.unwrap();
    let view = terminal(&session_id);
    service
        .publish(
            &session_id,
            SessionPresentationEvent::TerminalAdded {
                terminal: view.clone(),
            },
        )
        .unwrap();
    let current = service.snapshot(&session_id).await.unwrap();
    assert!(
        current.terminals.is_empty(),
        "backend snapshots remain authoritative"
    );

    let encoded = serde_json::to_string(&SessionPresentationEvent::TerminalCommandStarted {
        terminal_id: view.terminal_id,
        sequence: 1,
    })
    .unwrap();
    assert!(!encoded.contains("output"));
    assert!(!encoded.contains("\"command\":"));
}

fn prompt(session_id: &SessionId, submission_id: &str) -> PromptSubmission {
    PromptSubmission {
        session_id: session_id.clone(),
        client_submission_id: submission_id.to_owned(),
        content: Content::text(submission_id).unwrap(),
        budget: PromptBudget::default(),
    }
}

fn human_terminal_ensure(profile: ExecutionProfile) -> HumanTerminalEnsure {
    HumanTerminalEnsure {
        session_id: SessionId::generate(),
        client_submission_id: "submission".to_owned(),
        execution_context_revision: 1,
        profile,
        shell_profile_id: "bash-managed".to_owned(),
        terminal_size: TerminalSize::default(),
        agl_env: StructuredEnvironmentOverlay::default(),
        host_startup: HostStartupPolicy::ManagedOnly,
    }
}

fn terminal(session_id: &SessionId) -> TerminalSessionView {
    TerminalSessionView {
        terminal_id: TerminalSessionId::generate(),
        execution_id: ExecutionId::generate(),
        owner: TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        profile: ExecutionProfile::Workspace,
        shell: ShellProfileView {
            profile_id: "bash-managed".to_owned(),
            program: "/bin/bash".to_owned(),
            executable_digest: "sha256:executable".to_owned(),
            config_digest: "sha256:config".to_owned(),
        },
        workspace_root: "/workspace".to_owned(),
        cwd: "/workspace".to_owned(),
        initial_environment_digest: "sha256:environment".to_owned(),
        environment_names: vec!["PATH".to_owned()],
        command_sequence: 0,
        prompt_state: TerminalPromptState::Ready,
        process_state: ExecutionState::Running,
        exit: None,
        writer: TerminalWriterView::Owner,
        promoted: false,
    }
}

fn snapshot(
    daemon_instance_id: &DaemonInstanceId,
    session_id: &SessionId,
) -> SessionPresentationSnapshot {
    let command_context = CommandContext {
        session_id: Some(session_id.clone()),
        session_active: true,
        ..CommandContext::default()
    };
    SessionPresentationSnapshot {
        session_id: session_id.clone(),
        cursor: PresentationCursor {
            daemon_instance_id: daemon_instance_id.clone(),
            revision: 0,
        },
        header: SessionHeader {
            session_id: session_id.clone(),
            status: SessionPresentationStatus::Active,
            durable: true,
            resumed: false,
            title: None,
            function_name: "agent".to_owned(),
            model_id: None,
            operation_mode: ToolAccessMode::ReadOnly,
            selected_skills: Vec::new(),
            runtime_context_revision: 0,
            workspace_root: "/workspace".to_owned(),
            cwd: "/workspace".to_owned(),
            execution_context_revision: 0,
            context_used_tokens: None,
            context_limit_tokens: None,
            active_run_count: 0,
            queued_prompt_count: 0,
            active_execution_count: 0,
        },
        items: Vec::new(),
        active_run: None,
        queued_prompts: Vec::new(),
        terminals: Vec::new(),
        executions: Vec::new(),
        command_context,
    }
}

struct FakeBackend {
    snapshot: SessionPresentationSnapshot,
    older_page_cursor: Option<String>,
    exit_on_invoke: bool,
}

struct SequencedSnapshotBackend {
    snapshots: Mutex<VecDeque<SessionPresentationSnapshot>>,
    admission: Option<PromptAdmission>,
}

struct BlockingSnapshotBackend {
    snapshot: SessionPresentationSnapshot,
    entered: AtomicUsize,
    cancelled: AtomicUsize,
    release: AtomicBool,
}

impl ApplicationBackend for BlockingSnapshotBackend {
    fn open_session(
        &self,
        _context: ApplicationCallContext,
        _request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn snapshot_page(
        &self,
        context: ApplicationCallContext,
        _session_id: &SessionId,
        _page_cursor: Option<&str>,
    ) -> Result<PresentationSnapshotPage, ApplicationError> {
        self.entered.fetch_add(1, Ordering::AcqRel);
        while !self.release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        if context.is_cancelled() {
            self.cancelled.fetch_add(1, Ordering::AcqRel);
        }
        Ok(PresentationSnapshotPage {
            snapshot: self.snapshot.clone(),
            older_page_cursor: None,
        })
    }

    fn invoke(
        &self,
        _context: ApplicationCallContext,
        _request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn submit_prompt(
        &self,
        _context: ApplicationCallContext,
        _request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn ensure_human_terminal(
        &self,
        _context: ApplicationCallContext,
        _request: HumanTerminalEnsure,
    ) -> Result<TerminalEnsured, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn suggestions(
        &self,
        _context: ApplicationCallContext,
        _request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }
}

impl ApplicationBackend for SequencedSnapshotBackend {
    fn open_session(
        &self,
        _context: ApplicationCallContext,
        _request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn snapshot_page(
        &self,
        _context: ApplicationCallContext,
        _session_id: &SessionId,
        _page_cursor: Option<&str>,
    ) -> Result<PresentationSnapshotPage, ApplicationError> {
        let mut snapshots = self.snapshots.lock().unwrap();
        let snapshot = if snapshots.len() > 1 {
            snapshots.pop_front().unwrap()
        } else {
            snapshots.front().unwrap().clone()
        };
        Ok(PresentationSnapshotPage {
            snapshot,
            older_page_cursor: None,
        })
    }

    fn invoke(
        &self,
        _context: ApplicationCallContext,
        _request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn submit_prompt(
        &self,
        _context: ApplicationCallContext,
        _request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        self.admission.clone().ok_or_else(|| {
            ApplicationError::new(ApplicationErrorCode::CommandUnavailable, "not used")
        })
    }

    fn ensure_human_terminal(
        &self,
        _context: ApplicationCallContext,
        _request: HumanTerminalEnsure,
    ) -> Result<TerminalEnsured, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn suggestions(
        &self,
        _context: ApplicationCallContext,
        _request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }
}

impl ApplicationBackend for FakeBackend {
    fn open_session(
        &self,
        _context: crate::ApplicationCallContext,
        _request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError> {
        Ok(SessionOpened {
            session_id: self.snapshot.session_id.clone(),
            resumed: false,
            snapshot: self.snapshot.clone(),
        })
    }

    fn snapshot_page(
        &self,
        _context: crate::ApplicationCallContext,
        _session_id: &SessionId,
        _page_cursor: Option<&str>,
    ) -> Result<PresentationSnapshotPage, ApplicationError> {
        Ok(PresentationSnapshotPage {
            snapshot: self.snapshot.clone(),
            older_page_cursor: self.older_page_cursor.clone(),
        })
    }

    fn invoke(
        &self,
        _context: crate::ApplicationCallContext,
        _request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError> {
        if self.exit_on_invoke {
            return Ok(ApplicationActionResult::SessionExited {
                session_id: self.snapshot.session_id.clone(),
                cancelled_runs: 0,
                terminated_terminals: 0,
                terminated_executions: 0,
            });
        }
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn submit_prompt(
        &self,
        _context: crate::ApplicationCallContext,
        _request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn ensure_human_terminal(
        &self,
        _context: crate::ApplicationCallContext,
        _request: HumanTerminalEnsure,
    ) -> Result<TerminalEnsured, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn suggestions(
        &self,
        _context: crate::ApplicationCallContext,
        _request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        Ok(SuggestionPage {
            entries: Vec::new(),
            next_cursor: None,
        })
    }
}
