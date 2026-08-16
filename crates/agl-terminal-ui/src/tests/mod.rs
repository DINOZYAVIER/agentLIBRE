use super::*;

mod commands;
mod history;
mod picker;
mod presentation;
mod render;
mod runtime;
mod session;
mod shell_submission;
mod state;
mod terminal_mode;

fn test_display_path(text: &str) -> agl_protocol::SanitizedDisplayPath {
    agl_protocol::SanitizedDisplayPath {
        text: text.to_owned(),
        truncated: false,
    }
}

pub(super) fn test_ui_state(session_id: SessionId, terminals: Vec<TerminalSessionView>) -> UiState {
    let last_terminal = terminals
        .first()
        .map(|terminal| terminal.terminal_id.clone());
    UiState {
        snapshot: SessionPresentationSnapshot {
            session_id: session_id.clone(),
            cursor: agl_protocol::PresentationCursor {
                daemon_instance_id: agl_ids::DaemonInstanceId::generate(),
                revision: 1,
            },
            older_page_cursor: None,
            header: agl_protocol::SessionHeader {
                session_id: session_id.clone(),
                status: agl_protocol::SessionPresentationStatus::Active,
                durable: true,
                resumed: false,
                title: None,
                function_name: "coding".to_owned(),
                model_id: Some("local".to_owned()),
                operation_mode: ProtocolToolMode::Execute,
                selected_skills: Vec::new(),
                runtime_context_revision: 1,
                workspace_root: test_display_path("/workspace"),
                cwd: test_display_path("/workspace"),
                workspace_history_scope: format!("sha256:{}", "a".repeat(64)),
                execution_context_revision: 41,
                context_used_tokens: None,
                context_limit_tokens: None,
                active_run_count: 0,
                queued_prompt_count: 0,
                active_execution_count: u32::try_from(terminals.len()).unwrap(),
            },
            items: Vec::new(),
            active_run: None,
            queued_prompts: Vec::new(),
            terminals,
            executions: Vec::new(),
            activity: None,
            command_context: agl_protocol::CommandContext {
                session_id: Some(session_id),
                session_active: true,
                active_or_queued_turns: 0,
                active_executions: 0,
                host_shell_available: true,
                operation_mode: ProtocolToolMode::Execute,
            },
        },
        catalog: Vec::new(),
        composer: Composer::default(),
        last_terminal,
        terminal_cursors: BTreeMap::new(),
        seen_terminals: BTreeSet::new(),
        assistant_deltas: BTreeMap::new(),
        continuation_submission_ids: BTreeMap::new(),
        picker: None,
        notices: Vec::new(),
        active_run: None,
        exit_armed: false,
        workspace_change_armed: None,
        shell_profile_id: Some("bash-managed".to_owned()),
        history: InputHistory {
            root: None,
            prompt: Vec::new(),
        },
        activity_expanded: false,
        pending_shell_submission: None,
        human_commands: Vec::new(),
        no_color: false,
    }
}

fn test_process(terminal: Option<TerminalSessionView>) -> ProcessPickerItem {
    let execution_id = terminal
        .as_ref()
        .map(|terminal| terminal.execution_id.clone())
        .unwrap_or_else(ExecutionId::generate);
    ProcessPickerItem {
        execution_id,
        state: agl_protocol::ExecutionState::Running,
        profile: terminal
            .as_ref()
            .map_or(ExecutionProfile::Workspace, |terminal| terminal.profile),
        cwd: "/workspace".to_owned(),
        terminal,
    }
}

fn test_terminal(owner: TerminalOwnerView, profile: ExecutionProfile) -> TerminalSessionView {
    TerminalSessionView {
        terminal_id: TerminalId::generate(),
        execution_id: ExecutionId::generate(),
        owner,
        profile,
        shell: agl_protocol::ShellProfileView {
            profile_id: "bash-managed".to_owned(),
            program: test_display_path("/bin/bash"),
            executable_digest: "sha256:executable".to_owned(),
            config_digest: "sha256:config".to_owned(),
        },
        workspace_root: test_display_path("/workspace"),
        cwd: test_display_path("/workspace"),
        initial_environment_digest: "sha256:environment".to_owned(),
        environment_names: vec!["PATH".to_owned()],
        command_sequence: 0,
        prompt_generation: Some(1),
        prompt_state: TerminalPromptState::Ready,
        process_state: agl_protocol::ExecutionState::Running,
        exit: None,
        writer: agl_protocol::TerminalWriterView::Owner,
        promoted: false,
    }
}
