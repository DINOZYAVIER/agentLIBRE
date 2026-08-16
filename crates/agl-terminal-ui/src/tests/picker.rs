use super::*;

#[test]
fn picker_filter_and_selection_reducer_are_bounded() {
    let mut picker = PickerState::new(
        PickerKind::Model,
        "models",
        vec![
            PickerEntry {
                value: "local-small".to_owned(),
                label: "Small".to_owned(),
                detail: Some("fast local model".to_owned()),
                payload: PickerPayload::Model("local-small".to_owned()),
            },
            PickerEntry {
                value: "local-large".to_owned(),
                label: "Large".to_owned(),
                detail: Some("deep reasoning".to_owned()),
                payload: PickerPayload::Model("local-large".to_owned()),
            },
        ],
    );

    for character in "reason".chars() {
        picker.push_query(character);
    }
    assert_eq!(picker.filtered_indices(), vec![1]);
    assert_eq!(picker.selected_entry().unwrap().value, "local-large");

    picker.query.clear();
    picker.move_selection(50);
    assert_eq!(picker.selected_entry().unwrap().value, "local-large");
    picker.move_selection(-50);
    assert_eq!(picker.selected_entry().unwrap().value, "local-small");

    picker.query.clear();
    for _ in 0..512 {
        picker.push_query('a');
    }
    picker.push_query('b');
    picker.push_query('\n');
    assert_eq!(picker.query.len(), 512);
    assert!(!picker.query.ends_with('b'));
    assert!(!picker.query.contains('\n'));
}

#[test]
fn skills_picker_has_explicit_multi_select_and_empty_apply() {
    let entries = ["build", "review"]
        .into_iter()
        .map(|skill_id| PickerEntry {
            value: skill_id.to_owned(),
            label: skill_id.to_owned(),
            detail: None,
            payload: PickerPayload::Skill(skill_id.to_owned()),
        })
        .collect();
    let mut picker = PickerState::new(PickerKind::Skills, "skills", entries);

    picker.toggle_selected_skill();
    assert_eq!(picker.selected_values, BTreeSet::from(["build".to_owned()]));
    picker.select_all_skills();
    assert_eq!(
        picker.selected_values,
        BTreeSet::from(["build".to_owned(), "review".to_owned()])
    );
    picker.clear_skills();
    assert_eq!(
        picker_default_submit(&picker).unwrap(),
        PickerSubmit::Skills(Vec::new())
    );
}

#[test]
fn mode_picker_uses_canonical_values_and_typed_payloads() {
    let entries = operation_mode_picker_entries(ProtocolToolMode::ReadOnly);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.value.as_str())
            .collect::<Vec<_>>(),
        vec!["read-only", "write", "execute", "approve", "admin"]
    );
    assert_eq!(entries[0].detail.as_deref(), Some("current mode"));
    let picker = PickerState::new(PickerKind::Mode, "mode", entries);
    assert_eq!(
        picker_default_submit(&picker).unwrap(),
        PickerSubmit::Mode(ProtocolToolMode::ReadOnly)
    );
}

#[test]
fn process_picker_defaults_to_owner_write_and_foreign_read_only() {
    let human = test_terminal(
        TerminalOwnerView::Human {
            session_id: SessionId::generate(),
        },
        ExecutionProfile::Workspace,
    );
    let human_picker = PickerState::new(
        PickerKind::Processes,
        "processes",
        vec![process_picker_entry(test_process(Some(human.clone())))],
    );
    assert_eq!(
        picker_default_submit(&human_picker).unwrap(),
        PickerSubmit::Attach {
            terminal: Box::new(human),
            writable: true,
        }
    );

    let host = test_terminal(
        TerminalOwnerView::MainAgent {
            session_id: SessionId::generate(),
        },
        ExecutionProfile::Host,
    );
    let host_entry = process_picker_entry(test_process(Some(host.clone())));
    assert!(host_entry.detail.as_deref().unwrap().contains("HOST"));
    let foreign_picker = PickerState::new(PickerKind::Processes, "processes", vec![host_entry]);
    assert_eq!(
        picker_default_submit(&foreign_picker).unwrap(),
        PickerSubmit::Attach {
            terminal: Box::new(host),
            writable: false,
        }
    );
}

#[test]
fn host_picker_actions_require_confirmation_and_cancel_never_submits() {
    let session_id = SessionId::generate();
    let mut state = test_ui_state(session_id, Vec::new());
    state.picker = Some(PickerState::new(
        PickerKind::Processes,
        "processes",
        host_terminal_picker_entries(),
    ));
    let client = RecordingHostClient::error("must not be called");

    assert!(
        handle_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        )
        .is_none()
    );
    let confirmation = state
        .picker
        .as_ref()
        .and_then(|picker| picker.confirmation.as_ref())
        .expect("managed HOST action must enter confirmation state");
    assert!(confirmation.prompt.contains("managed startup"));
    assert!(matches!(
        confirmation.submit,
        PickerSubmit::EnsureHost {
            startup: HostStartupPolicy::ManagedOnly
        }
    ));
    assert!(client.requests().is_empty());

    assert!(
        handle_picker_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).is_none()
    );
    assert!(state.picker.as_ref().unwrap().confirmation.is_none());
    assert!(client.requests().is_empty());

    state.picker.as_mut().unwrap().selected = 1;
    assert!(
        handle_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        )
        .is_none()
    );
    let confirmation = state
        .picker
        .as_ref()
        .and_then(|picker| picker.confirmation.as_ref())
        .expect("user-rc HOST action must enter its own confirmation state");
    assert!(confirmation.prompt.contains("source your normal shell rc"));
    let Some(UiControl::Submission(ComposerSubmission::Picker(PickerSubmit::EnsureHost {
        startup,
    }))) = handle_picker_key(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    else {
        panic!("confirmed user-rc HOST action did not produce a typed submission");
    };
    assert_eq!(startup, HostStartupPolicy::SourceUserRc);
    assert!(client.requests().is_empty());
}

#[tokio::test]
async fn confirmed_managed_host_uses_explicit_family_and_attaches_distinct_terminal() {
    let session_id = SessionId::generate();
    let workspace = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Workspace,
    );
    let host = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Host,
    );
    let client = RecordingHostClient::success(
        host.clone(),
        agl_protocol::TerminalEnsureDisposition::Created,
    );
    let mut state = test_ui_state(session_id.clone(), vec![workspace.clone()]);

    let outcome = handle_host_terminal_submit(
        &client,
        &session_id,
        &mut state,
        HostStartupPolicy::ManagedOnly,
    )
    .await;
    let SubmissionOutcome::EnterTerminal(request) = outcome else {
        panic!("confirmed HOST ensure did not enter Terminal view");
    };
    assert!(request.writable);
    assert_eq!(request.terminal, host);
    assert_eq!(terminal_authority_label(request.terminal.profile), "HOST");
    assert_eq!(
        state.last_terminal,
        Some(request.terminal.terminal_id.clone())
    );
    assert_eq!(state.snapshot.terminals[0], workspace);
    assert_eq!(state.snapshot.terminals.len(), 2);

    let requests = client.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.terminal.session_id, session_id);
    assert_eq!(request.terminal.profile, ExecutionProfile::Host);
    assert_eq!(
        request.terminal.host_startup,
        HostStartupPolicy::ManagedOnly
    );
    assert!(request.confirm_host_authority);
    assert_eq!(request.terminal.execution_context_revision, 41);
    assert_eq!(request.terminal.shell_profile_id, "bash-managed");
    assert_eq!(request.terminal.agl_env, current_terminal_environment());
    assert!(
        request
            .terminal
            .client_submission_id
            .starts_with("cli-host-terminal-")
    );
    let wire = serde_json::to_value(request).unwrap();
    assert!(wire.get("path").is_none());
    assert!(!wire.to_string().contains(".bashrc"));
    assert!(!wire.to_string().contains(".zshrc"));
}

#[tokio::test]
async fn source_user_rc_request_is_explicit_and_existing_host_is_idempotent() {
    let session_id = SessionId::generate();
    let workspace = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Workspace,
    );
    let host = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Host,
    );
    let client = RecordingHostClient::success(
        host.clone(),
        agl_protocol::TerminalEnsureDisposition::Reused,
    );
    let mut state = test_ui_state(session_id.clone(), vec![workspace.clone(), host.clone()]);

    let outcome = handle_host_terminal_submit(
        &client,
        &session_id,
        &mut state,
        HostStartupPolicy::SourceUserRc,
    )
    .await;
    let SubmissionOutcome::EnterTerminal(request) = outcome else {
        panic!("reused HOST terminal was not attached");
    };
    assert!(request.writable);
    assert_eq!(request.terminal.terminal_id, host.terminal_id);
    assert_eq!(state.snapshot.terminals[0], workspace);
    assert_eq!(
        state
            .snapshot
            .terminals
            .iter()
            .filter(|terminal| terminal.terminal_id == host.terminal_id)
            .count(),
        1
    );
    let requests = client.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].terminal.host_startup,
        HostStartupPolicy::SourceUserRc
    );
    assert!(requests[0].confirm_host_authority);
}

#[tokio::test]
async fn host_errors_are_visible_and_workspace_identity_cannot_be_upgraded() {
    let session_id = SessionId::generate();
    let workspace = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Workspace,
    );
    let client = RecordingHostClient::error("operator denied Host authority");
    let mut state = test_ui_state(session_id.clone(), vec![workspace.clone()]);
    assert!(matches!(
        handle_host_terminal_submit(
            &client,
            &session_id,
            &mut state,
            HostStartupPolicy::ManagedOnly
        )
        .await,
        SubmissionOutcome::Continue
    ));
    assert!(
        state
            .notices
            .iter()
            .any(|notice| notice.contains("operator denied Host authority"))
    );
    assert_eq!(state.snapshot.terminals, vec![workspace.clone()]);

    let mut invalid_host = test_terminal(
        TerminalOwnerView::Human {
            session_id: session_id.clone(),
        },
        ExecutionProfile::Host,
    );
    invalid_host.terminal_id = workspace.terminal_id.clone();
    invalid_host.execution_id = workspace.execution_id.clone();
    let client = RecordingHostClient::success(
        invalid_host,
        agl_protocol::TerminalEnsureDisposition::Created,
    );
    let mut state = test_ui_state(session_id.clone(), vec![workspace.clone()]);
    assert!(matches!(
        handle_host_terminal_submit(
            &client,
            &session_id,
            &mut state,
            HostStartupPolicy::ManagedOnly
        )
        .await,
        SubmissionOutcome::Continue
    ));
    assert!(
        state
            .notices
            .iter()
            .any(|notice| notice.contains("reuse a Workspace terminal identity"))
    );
    assert_eq!(state.snapshot.terminals, vec![workspace.clone()]);
    assert_eq!(state.last_terminal, Some(workspace.terminal_id));
}

struct RecordingHostClient {
    response: std::result::Result<HumanTerminalEnsuredEvent, ClientError>,
    requests: Mutex<Vec<HumanHostTerminalEnsureRequest>>,
}

impl RecordingHostClient {
    fn success(
        terminal: TerminalSessionView,
        disposition: agl_protocol::TerminalEnsureDisposition,
    ) -> Self {
        Self {
            response: Ok(HumanTerminalEnsuredEvent {
                terminal,
                disposition,
            }),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn error(message: &str) -> Self {
        Self {
            response: Err(ClientError::InvalidRequest(message.to_owned())),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<HumanHostTerminalEnsureRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl HostTerminalEnsurer for RecordingHostClient {
    async fn ensure_host_terminal(
        &self,
        request: HumanHostTerminalEnsureRequest,
    ) -> std::result::Result<HumanTerminalEnsuredEvent, ClientError> {
        self.requests.lock().unwrap().push(request);
        self.response.clone()
    }
}
