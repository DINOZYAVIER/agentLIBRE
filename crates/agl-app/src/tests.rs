use std::collections::BTreeSet;
use std::sync::Arc;

use agl_capabilities::ToolAccessMode;
use agl_content::Content;
use agl_ids::{DaemonInstanceId, RunId, SessionId};
use agl_process::{ExecutionProfile, TerminalSize};

use super::*;

#[test]
fn command_catalog_has_unique_ids_names_and_busy_mutation_availability() {
    let catalog = shared_command_catalog(&CommandContext {
        session_id: Some(SessionId::generate()),
        session_active: true,
        active_or_queued_turns: 1,
        active_executions: 1,
        host_shell_available: false,
        operation_mode: ToolAccessMode::Execute,
    });
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
    let model = catalog
        .descriptors
        .iter()
        .find(|descriptor| descriptor.id.as_str() == "model.select")
        .unwrap();
    assert!(matches!(
        model.availability,
        CommandAvailability::Disabled { ref reason_code, .. } if reason_code == "session_busy"
    ));
    let exit = catalog
        .descriptors
        .iter()
        .find(|descriptor| descriptor.id.as_str() == "session.exit")
        .unwrap();
    assert_eq!(exit.availability, CommandAvailability::Enabled);
    assert!(!names.contains("finish"));
    assert!(!names.contains("quit"));
}

#[test]
fn prompt_queue_is_fifo_bounded_and_submission_idempotent() {
    let session_id = SessionId::generate();
    let mut queue = PromptQueue::default();
    let first_submission = prompt(&session_id, "first");
    let first = queue.admit(&first_submission, RunId::generate()).unwrap();
    let replay = queue.admit(&first_submission, RunId::generate()).unwrap();
    assert_eq!(replay.run_id, first.run_id);
    assert!(!first.queued);

    let second = queue
        .admit(&prompt(&session_id, "second"), RunId::generate())
        .unwrap();
    assert!(second.queued);
    assert_eq!(
        queue.finish(&session_id, &first.run_id).unwrap().run_id,
        second.run_id
    );
}

#[test]
fn user_shell_validation_keeps_command_opaque_and_rejects_empty_or_nul() {
    let mut submission = shell_submission("printf '%s' '$() ; | &'".to_owned());
    submission.validate().unwrap();
    assert_eq!(submission.command, "printf '%s' '$() ; | &'");
    submission.command = "\n\r".to_owned();
    assert_eq!(
        submission.validate().unwrap_err().code,
        ApplicationErrorCode::InvalidArguments
    );
    submission.command = "echo\0bad".to_owned();
    assert!(submission.validate().is_err());
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

fn prompt(session_id: &SessionId, submission_id: &str) -> PromptSubmission {
    PromptSubmission {
        session_id: session_id.clone(),
        client_submission_id: submission_id.to_owned(),
        content: Content::text(submission_id).unwrap(),
    }
}

fn shell_submission(command: String) -> UserShellSubmission {
    UserShellSubmission {
        session_id: SessionId::generate(),
        client_submission_id: "submission".to_owned(),
        command,
        execution_context_revision: 1,
        profile: ExecutionProfile::Workspace,
        terminal_size: TerminalSize::default(),
        background: false,
        operator: LocalOperatorPrincipal { uid: 1000 },
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
        executions: Vec::new(),
        command_context,
    }
}

struct FakeBackend {
    snapshot: SessionPresentationSnapshot,
}

impl ApplicationBackend for FakeBackend {
    fn open_session(&self, _request: SessionOpen) -> Result<SessionOpened, ApplicationError> {
        Ok(SessionOpened {
            session_id: self.snapshot.session_id.clone(),
            resumed: false,
            snapshot: self.snapshot.clone(),
        })
    }

    fn snapshot(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionPresentationSnapshot, ApplicationError> {
        Ok(self.snapshot.clone())
    }

    fn invoke(
        &self,
        _request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn submit_prompt(
        &self,
        _request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn start_user_shell(
        &self,
        _request: UserShellSubmission,
    ) -> Result<UserShellAdmission, ApplicationError> {
        Err(ApplicationError::new(
            ApplicationErrorCode::CommandUnavailable,
            "not used",
        ))
    }

    fn suggestions(&self, _request: SuggestionRequest) -> Result<SuggestionPage, ApplicationError> {
        Ok(SuggestionPage {
            entries: Vec::new(),
            next_cursor: None,
        })
    }
}
