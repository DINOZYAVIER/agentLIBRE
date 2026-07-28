use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};

use agl_ids::{MessageId, SessionId, TerminalSessionId};
use agl_kernel::ToolAccessMode;
use agl_process::{ExecutionId, KillMode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ApplicationError, ApplicationErrorCode};

pub const MAX_COMMAND_DESCRIPTORS: usize = 256;
pub const MAX_COMMAND_ARGUMENTS: usize = 64;
pub const MAX_ACTION_STRING_BYTES: usize = 8 * 1024;
pub const MAX_SELECTED_SKILLS: usize = 128;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommandId(String);

impl CommandId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ApplicationError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value.split('.').all(|part| {
                !part.is_empty()
                    && part.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            });
        if !valid {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "command ID must be a bounded lowercase dotted identifier",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for CommandId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for CommandId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CommandId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    Session,
    Runtime,
    Workspace,
    Execution,
    Client,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandArgumentKind {
    String,
    Boolean,
    Unsigned,
    Path,
    SessionId,
    ExecutionId,
    ModelId,
    OperationMode,
    SkillId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgumentDescriptor {
    pub id: String,
    pub label: String,
    pub kind: CommandArgumentKind,
    pub required: bool,
    pub repeated: bool,
    pub suggestion_source: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationActionKind {
    ClientHelp,
    ClientDisconnect,
    SessionNew,
    SessionResume,
    SessionStatus,
    ModelSelect,
    OperationModeSelect,
    SkillsSelect,
    WorkspaceGet,
    WorkspaceSet,
    TerminalList,
    TerminalPromote,
    IncompleteTurnContinue,
    ExecutionList,
    ExecutionAttach,
    ExecutionKill,
    RuntimeContextReload,
    SessionClear,
    SessionExit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandConcurrency {
    ReadOnly,
    TurnBoundaryMutation,
    SessionDestructive,
    StartsExecution,
    SurfaceLocal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandAvailability {
    Enabled,
    Disabled {
        reason_code: String,
        message: String,
    },
    Hidden,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDescriptor {
    pub id: CommandId,
    pub name: String,
    pub aliases: Vec<String>,
    pub summary: String,
    pub category: CommandCategory,
    pub arguments: Vec<CommandArgumentDescriptor>,
    pub action_kind: ApplicationActionKind,
    pub concurrency: CommandConcurrency,
    pub availability: CommandAvailability,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCatalog {
    pub descriptors: Vec<CommandDescriptor>,
}

impl CommandCatalog {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.descriptors.len() > MAX_COMMAND_DESCRIPTORS {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "command catalog exceeds its descriptor bound",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        for descriptor in &self.descriptors {
            if descriptor.arguments.len() > MAX_COMMAND_ARGUMENTS
                || descriptor.name.is_empty()
                || descriptor.name.len() > 128
                || !descriptor
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                || !ids.insert(descriptor.id.as_str())
                || !names.insert(descriptor.name.as_str())
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "command descriptors must be bounded with unique IDs and names",
                ));
            }
            for alias in &descriptor.aliases {
                if alias.is_empty()
                    || alias.len() > 128
                    || !alias.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                    || !names.insert(alias.as_str())
                {
                    return Err(ApplicationError::new(
                        ApplicationErrorCode::InvalidArguments,
                        "command aliases must be bounded and globally unique",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandContext {
    pub session_id: Option<SessionId>,
    pub session_active: bool,
    pub active_or_queued_turns: u32,
    pub active_executions: u32,
    pub host_shell_available: bool,
    pub operation_mode: ToolAccessMode,
}

impl Default for CommandContext {
    fn default() -> Self {
        Self {
            session_id: None,
            session_active: false,
            active_or_queued_turns: 0,
            active_executions: 0,
            host_shell_available: false,
            operation_mode: ToolAccessMode::ReadOnly,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLaunchOptions {
    pub workspace_root: Option<String>,
    pub function_ref: Option<String>,
    pub model_id: Option<String>,
    pub operation_mode: Option<ToolAccessMode>,
    pub skill_ids: Vec<String>,
}

impl SessionLaunchOptions {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.skill_ids.len() > MAX_SELECTED_SKILLS {
            return invalid_action("session launch contains too many selected skills");
        }
        for (label, value) in [
            ("workspace root", self.workspace_root.as_deref()),
            ("function reference", self.function_ref.as_deref()),
            ("model ID", self.model_id.as_deref()),
        ] {
            if let Some(value) = value {
                validate_action_text(value, label)?;
            }
        }
        for skill_id in &self.skill_ids {
            validate_action_text(skill_id, "skill ID")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionSelector {
    Latest,
    Id { session_id: SessionId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationAction {
    SessionNew {
        launch: SessionLaunchOptions,
    },
    SessionResume {
        selector: SessionSelector,
    },
    SessionStatus,
    ModelSelect {
        model_id: String,
    },
    OperationModeSelect {
        mode: ToolAccessMode,
    },
    SkillsSelect {
        skill_ids: Vec<String>,
    },
    WorkspaceGet,
    WorkspaceSet {
        path: String,
        confirm_terminate_terminals: bool,
    },
    TerminalList {
        include_finished: bool,
    },
    TerminalPromote {
        terminal_id: TerminalSessionId,
    },
    IncompleteTurnContinue {
        message_id: MessageId,
        expected_execution_context_revision: u64,
    },
    ExecutionList {
        include_finished: bool,
    },
    ExecutionAttach {
        execution_id: ExecutionId,
        read_only: bool,
    },
    ExecutionKill {
        execution_id: ExecutionId,
        mode: KillMode,
    },
    RuntimeContextReload,
    SessionClear,
    SessionExit {
        confirm_active: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationActionRequest {
    pub session_id: Option<SessionId>,
    pub client_submission_id: String,
    pub action: ApplicationAction,
}

impl ApplicationActionRequest {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        validate_action_text(&self.client_submission_id, "client submission ID")?;
        match &self.action {
            ApplicationAction::SessionNew { launch } => launch.validate()?,
            ApplicationAction::ModelSelect { model_id } => {
                validate_action_text(model_id, "model ID")?
            }
            ApplicationAction::SkillsSelect { skill_ids } => {
                if skill_ids.len() > MAX_SELECTED_SKILLS {
                    return invalid_action("action contains too many selected skills");
                }
                for skill_id in skill_ids {
                    validate_action_text(skill_id, "skill ID")?;
                }
            }
            ApplicationAction::WorkspaceSet { path, .. } => {
                validate_action_text(path, "workspace path")?
            }
            ApplicationAction::SessionResume { .. }
            | ApplicationAction::SessionStatus
            | ApplicationAction::OperationModeSelect { .. }
            | ApplicationAction::WorkspaceGet
            | ApplicationAction::TerminalList { .. }
            | ApplicationAction::TerminalPromote { .. }
            | ApplicationAction::IncompleteTurnContinue { .. }
            | ApplicationAction::ExecutionList { .. }
            | ApplicationAction::ExecutionAttach { .. }
            | ApplicationAction::ExecutionKill { .. }
            | ApplicationAction::RuntimeContextReload
            | ApplicationAction::SessionClear
            | ApplicationAction::SessionExit { .. } => {}
        }
        Ok(())
    }
}

fn validate_action_text(value: &str, label: &str) -> Result<(), ApplicationError> {
    if value.is_empty()
        || value.len() > MAX_ACTION_STRING_BYTES
        || value.contains(['\0', '\n', '\r'])
    {
        return invalid_action(format!("{label} must be nonempty bounded single-line text"));
    }
    Ok(())
}

fn invalid_action<T>(message: impl Into<String>) -> Result<T, ApplicationError> {
    Err(ApplicationError::new(
        ApplicationErrorCode::InvalidArguments,
        message,
    ))
}

fn argument(
    id: &str,
    label: &str,
    kind: CommandArgumentKind,
    required: bool,
    repeated: bool,
    suggestion_source: Option<&str>,
) -> CommandArgumentDescriptor {
    CommandArgumentDescriptor {
        id: id.to_owned(),
        label: label.to_owned(),
        kind,
        required,
        repeated,
        suggestion_source: suggestion_source.map(str::to_owned),
    }
}

#[allow(clippy::too_many_arguments)]
fn descriptor(
    id: &str,
    name: &str,
    summary: &str,
    category: CommandCategory,
    arguments: Vec<CommandArgumentDescriptor>,
    action_kind: ApplicationActionKind,
    concurrency: CommandConcurrency,
    availability: CommandAvailability,
) -> CommandDescriptor {
    CommandDescriptor {
        id: CommandId::parse(id).expect("static command ID is valid"),
        name: name.to_owned(),
        aliases: Vec::new(),
        summary: summary.to_owned(),
        category,
        arguments,
        action_kind,
        concurrency,
        availability,
    }
}

pub fn shared_command_catalog(context: &CommandContext) -> CommandCatalog {
    let requires_session = || {
        if context.session_active {
            CommandAvailability::Enabled
        } else {
            CommandAvailability::Disabled {
                reason_code: "not_found".to_owned(),
                message: "open or resume a session first".to_owned(),
            }
        }
    };
    let turn_boundary = || {
        if !context.session_active {
            requires_session()
        } else if context.active_or_queued_turns > 0 {
            CommandAvailability::Disabled {
                reason_code: "session_busy".to_owned(),
                message: "wait for active and queued prompts to finish".to_owned(),
            }
        } else {
            CommandAvailability::Enabled
        }
    };
    let mut descriptors = vec![
        descriptor(
            "client.help",
            "help",
            "Search available commands",
            CommandCategory::Client,
            vec![argument(
                "filter",
                "filter",
                CommandArgumentKind::String,
                false,
                false,
                None,
            )],
            ApplicationActionKind::ClientHelp,
            CommandConcurrency::SurfaceLocal,
            CommandAvailability::Enabled,
        ),
        descriptor(
            "session.status",
            "status",
            "Show session status",
            CommandCategory::Session,
            vec![],
            ApplicationActionKind::SessionStatus,
            CommandConcurrency::ReadOnly,
            requires_session(),
        ),
        descriptor(
            "session.new",
            "new",
            "Open a new session",
            CommandCategory::Session,
            vec![],
            ApplicationActionKind::SessionNew,
            CommandConcurrency::TurnBoundaryMutation,
            CommandAvailability::Enabled,
        ),
        descriptor(
            "session.resume",
            "resume",
            "Resume a durable session",
            CommandCategory::Session,
            vec![argument(
                "selector",
                "latest or session ID",
                CommandArgumentKind::SessionId,
                false,
                false,
                Some("sessions"),
            )],
            ApplicationActionKind::SessionResume,
            CommandConcurrency::TurnBoundaryMutation,
            CommandAvailability::Enabled,
        ),
        descriptor(
            "model.select",
            "model",
            "Select an installed model",
            CommandCategory::Runtime,
            vec![argument(
                "model_id",
                "model",
                CommandArgumentKind::ModelId,
                false,
                false,
                Some("models"),
            )],
            ApplicationActionKind::ModelSelect,
            CommandConcurrency::TurnBoundaryMutation,
            turn_boundary(),
        ),
        descriptor(
            "mode.select",
            "mode",
            "Select operation mode",
            CommandCategory::Runtime,
            vec![argument(
                "mode",
                "mode",
                CommandArgumentKind::OperationMode,
                false,
                false,
                Some("modes"),
            )],
            ApplicationActionKind::OperationModeSelect,
            CommandConcurrency::TurnBoundaryMutation,
            turn_boundary(),
        ),
        descriptor(
            "skills.select",
            "skills",
            "Select admitted skills",
            CommandCategory::Runtime,
            vec![argument(
                "skill_id",
                "skill",
                CommandArgumentKind::SkillId,
                false,
                true,
                Some("skills"),
            )],
            ApplicationActionKind::SkillsSelect,
            CommandConcurrency::TurnBoundaryMutation,
            turn_boundary(),
        ),
        descriptor(
            "workspace.root",
            "workspace",
            "Show or change workspace",
            CommandCategory::Workspace,
            vec![argument(
                "path",
                "path",
                CommandArgumentKind::Path,
                false,
                false,
                Some("paths"),
            )],
            ApplicationActionKind::WorkspaceSet,
            CommandConcurrency::TurnBoundaryMutation,
            turn_boundary(),
        ),
        descriptor(
            "execution.list",
            "processes",
            "List session executions",
            CommandCategory::Execution,
            vec![argument(
                "all",
                "include finished",
                CommandArgumentKind::Boolean,
                false,
                false,
                None,
            )],
            ApplicationActionKind::ExecutionList,
            CommandConcurrency::ReadOnly,
            requires_session(),
        ),
        descriptor(
            "execution.attach",
            "attach",
            "Attach to an execution terminal",
            CommandCategory::Execution,
            vec![
                argument(
                    "execution_id",
                    "execution",
                    CommandArgumentKind::ExecutionId,
                    true,
                    false,
                    Some("executions"),
                ),
                argument(
                    "read_only",
                    "read only",
                    CommandArgumentKind::Boolean,
                    false,
                    false,
                    None,
                ),
            ],
            ApplicationActionKind::ExecutionAttach,
            CommandConcurrency::StartsExecution,
            requires_session(),
        ),
        descriptor(
            "execution.kill",
            "kill",
            "Terminate an execution",
            CommandCategory::Execution,
            vec![
                argument(
                    "execution_id",
                    "execution",
                    CommandArgumentKind::ExecutionId,
                    true,
                    false,
                    Some("executions"),
                ),
                argument(
                    "immediate",
                    "immediate",
                    CommandArgumentKind::Boolean,
                    false,
                    false,
                    None,
                ),
            ],
            ApplicationActionKind::ExecutionKill,
            CommandConcurrency::SessionDestructive,
            requires_session(),
        ),
        descriptor(
            "session.reload",
            "reload",
            "Reload runtime context",
            CommandCategory::Runtime,
            vec![],
            ApplicationActionKind::RuntimeContextReload,
            CommandConcurrency::TurnBoundaryMutation,
            turn_boundary(),
        ),
        descriptor(
            "session.clear",
            "clear",
            "Clear durable conversation context",
            CommandCategory::Session,
            vec![],
            ApplicationActionKind::SessionClear,
            CommandConcurrency::TurnBoundaryMutation,
            turn_boundary(),
        ),
        descriptor(
            "session.exit",
            "exit",
            "Finish the durable session",
            CommandCategory::Session,
            vec![],
            ApplicationActionKind::SessionExit,
            CommandConcurrency::SessionDestructive,
            requires_session(),
        ),
        descriptor(
            "client.disconnect",
            "disconnect",
            "Disconnect this client without finishing the session",
            CommandCategory::Client,
            vec![],
            ApplicationActionKind::ClientDisconnect,
            CommandConcurrency::SurfaceLocal,
            CommandAvailability::Enabled,
        ),
    ];
    descriptors.sort_by(|left, right| left.id.cmp(&right.id));
    CommandCatalog { descriptors }
}
