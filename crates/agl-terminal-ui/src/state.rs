use super::*;

pub(super) struct InteractiveState {
    pub(super) snapshot: SessionPresentationSnapshot,
    pub(super) catalog: Vec<CommandDescriptor>,
    pub(super) composer: Composer,
    pub(super) last_terminal: Option<TerminalId>,
    pub(super) terminal_cursors: BTreeMap<ExecutionId, u64>,
    pub(super) seen_terminals: BTreeSet<TerminalId>,
    pub(super) assistant_deltas: BTreeMap<MessageId, AssistantDeltaState>,
    pub(super) continuation_submission_ids: BTreeMap<MessageId, String>,
    pub(super) picker: Option<PickerState>,
    pub(super) notices: Vec<String>,
    pub(super) active_run: Option<agl_ids::RunId>,
    pub(super) exit_armed: bool,
    pub(super) workspace_change_armed: Option<String>,
    pub(super) shell_profile_id: Option<String>,
    pub(super) history: InputHistory,
    pub(super) activity_expanded: bool,
    pub(super) pending_shell_submission: Option<PendingShellSubmission>,
    pub(super) human_commands: Vec<LocalHumanCommandCard>,
    pub(super) no_color: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalHumanCommandState {
    Running,
    Completed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LocalHumanCommandCard {
    pub(super) terminal_id: TerminalId,
    pub(super) command_sequence: u64,
    pub(super) command: String,
    pub(super) state: LocalHumanCommandState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingShellSubmission {
    pub(super) command: String,
    pub(super) client_submission_id: String,
    pub(super) terminal_ensure_submission_id: String,
    pub(super) in_flight: bool,
    pub(super) outcome_uncertain: bool,
}

#[derive(Clone)]
pub(super) struct ShellSubmissionTask {
    pub(super) session_id: SessionId,
    pub(super) command: String,
    pub(super) client_submission_id: String,
    pub(super) terminal_ensure_submission_id: String,
    pub(super) execution_context_revision: u64,
    pub(super) shell_profile_id: Option<String>,
    pub(super) terminal_size: TerminalSize,
    pub(super) agl_env: StructuredEnvironmentOverlay,
    pub(super) selected_terminal: Option<TerminalSessionView>,
    pub(super) attach_after_sequence: u64,
}

pub(super) struct ShellSubmissionAttachment {
    pub(super) terminal: TerminalSessionView,
    pub(super) attachment: ExecutionAttachment,
    pub(super) after_sequence: u64,
}

pub(super) struct ShellSubmissionFailure {
    pub(super) message: String,
    pub(super) outcome_uncertain: bool,
}

pub(super) struct ShellSubmissionCompletion {
    pub(super) session_id: SessionId,
    pub(super) command: String,
    pub(super) client_submission_id: String,
    pub(super) terminal: Option<TerminalSessionView>,
    pub(super) attachment: Option<ShellSubmissionAttachment>,
    pub(super) outcome: std::result::Result<ShellCommandAccepted, ShellSubmissionFailure>,
}

pub(super) struct ShellCommandAccepted {
    pub(super) terminal_id: TerminalId,
    pub(super) command_sequence: u64,
}

pub(super) struct AssistantDeltaState {
    pub(super) run_id: RunId,
    pub(super) next_sequence: u64,
    pub(super) text: String,
    pub(super) valid: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AssistantDeltaApply {
    Applied,
    Duplicate,
    SequenceGap,
    BoundExceeded,
}

impl InteractiveState {
    pub(super) fn latest_available_incomplete(&self) -> Option<MessageId> {
        self.snapshot.items.iter().rev().find_map(|item| {
            let SessionPresentationItem::IncompleteAssistant { item } = item else {
                return None;
            };
            matches!(
                item.continue_action,
                agl_protocol::ContinueActionView::Available
            )
            .then(|| item.message_id.clone())
        })
    }

    pub(super) fn matching_commands(&self) -> Vec<&CommandDescriptor> {
        let query = self
            .composer
            .buffer
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        self.catalog
            .iter()
            .filter(|command| {
                !matches!(command.availability, CommandAvailability::Hidden)
                    && (query.is_empty()
                        || command.name.starts_with(&query)
                        || command
                            .aliases
                            .iter()
                            .any(|alias| alias.starts_with(&query)))
            })
            .take(8)
            .collect()
    }

    pub(super) fn notice(&mut self, message: impl Into<String>) {
        self.notices.push(message.into());
        if self.notices.len() > 6 {
            self.notices.remove(0);
        }
    }
}

pub(super) type UiState = InteractiveState;

pub(super) enum UiAsyncEvent {
    RunAccepted {
        session_id: SessionId,
        run_id: agl_ids::RunId,
        state: ProtocolRunState,
    },
    Snapshot {
        session_id: SessionId,
        snapshot: Box<SessionPresentationSnapshot>,
    },
    ShellSubmission(Box<ShellSubmissionCompletion>),
    Notice(String),
}
