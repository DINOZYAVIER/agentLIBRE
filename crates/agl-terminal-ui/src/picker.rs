use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PickerKind {
    Resume,
    Model,
    Mode,
    Skills,
    Processes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProcessPickerItem {
    pub(super) execution_id: ExecutionId,
    pub(super) state: agl_protocol::ExecutionState,
    pub(super) profile: ExecutionProfile,
    pub(super) cwd: String,
    pub(super) terminal: Option<TerminalSessionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PickerPayload {
    Resume(SessionId),
    Model(String),
    Mode(ProtocolToolMode),
    Skill(String),
    EnsureHost(HostStartupPolicy),
    Process(Box<ProcessPickerItem>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PickerEntry {
    pub(super) value: String,
    pub(super) label: String,
    pub(super) detail: Option<String>,
    pub(super) payload: PickerPayload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PickerSubmit {
    Resume(SessionId),
    Model(String),
    Mode(ProtocolToolMode),
    Skills(Vec<String>),
    EnsureHost {
        startup: HostStartupPolicy,
    },
    Attach {
        terminal: Box<TerminalSessionView>,
        writable: bool,
    },
    Kill {
        execution_id: ExecutionId,
        mode: agl_exec::KillMode,
    },
    Promote {
        terminal_id: TerminalId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PickerConfirmation {
    pub(super) prompt: String,
    pub(super) submit: PickerSubmit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PickerState {
    pub(super) kind: PickerKind,
    pub(super) title: String,
    pub(super) entries: Vec<PickerEntry>,
    pub(super) query: String,
    pub(super) selected: usize,
    pub(super) selected_values: BTreeSet<String>,
    pub(super) confirmation: Option<PickerConfirmation>,
}

impl PickerState {
    pub(super) fn new(
        kind: PickerKind,
        title: impl Into<String>,
        entries: Vec<PickerEntry>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            entries,
            query: String::new(),
            selected: 0,
            selected_values: BTreeSet::new(),
            confirmation: None,
        }
    }

    pub(super) fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.to_ascii_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || entry.value.to_ascii_lowercase().contains(&query)
                    || entry.label.to_ascii_lowercase().contains(&query)
                    || entry
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.to_ascii_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(super) fn selected_entry(&self) -> Option<&PickerEntry> {
        let indices = self.filtered_indices();
        indices
            .get(self.selected.min(indices.len().saturating_sub(1)))
            .and_then(|index| self.entries.get(*index))
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let length = self.filtered_indices().len();
        if length == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(length - 1)
        };
    }

    pub(super) fn select_value(&mut self, value: &str) {
        if let Some(index) = self.entries.iter().position(|entry| entry.value == value) {
            self.selected = index;
        }
    }

    pub(super) fn push_query(&mut self, character: char) {
        if !character.is_control() && self.query.len().saturating_add(character.len_utf8()) <= 512 {
            self.query.push(character);
            self.selected = 0;
        }
    }

    pub(super) fn pop_query(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    pub(super) fn toggle_selected_skill(&mut self) {
        let Some(PickerEntry {
            payload: PickerPayload::Skill(skill_id),
            ..
        }) = self.selected_entry()
        else {
            return;
        };
        let skill_id = skill_id.clone();
        if !self.selected_values.remove(&skill_id) {
            self.selected_values.insert(skill_id);
        }
    }

    pub(super) fn select_all_skills(&mut self) {
        self.selected_values = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                PickerPayload::Skill(skill_id) => Some(skill_id.clone()),
                _ => None,
            })
            .collect();
    }

    pub(super) fn clear_skills(&mut self) {
        self.selected_values.clear();
    }
}
