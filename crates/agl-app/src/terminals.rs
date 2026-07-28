use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Formatter};

use agl_ids::{RunId, SessionId, TerminalSessionId};
use agl_process::{
    ExecutionExit, ExecutionId, ExecutionProfile, ExecutionState, TerminalSize, WriterLeaseId,
};
use serde::{Deserialize, Serialize};

use crate::{ApplicationError, ApplicationErrorCode, SanitizedDisplayPath};

pub const MAX_TERMINALS_PER_SESSION: usize = 128;
pub const MAX_CLIENT_SUBMISSION_ID_BYTES: usize = 256;
pub const MAX_SHELL_PROFILE_ID_BYTES: usize = 256;
pub const MAX_ENVIRONMENT_NAMES: usize = 256;
pub const MAX_ENVIRONMENT_NAME_BYTES: usize = 255;
pub const MAX_ENVIRONMENT_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_ENVIRONMENT_OVERLAY_BYTES: usize = 64 * 1024;
pub const MAX_SECRET_REFERENCE_BYTES: usize = 1024;
pub const MAX_TERMINAL_PATH_BYTES: usize = 8 * 1024;
pub const MAX_DIGEST_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStartupPolicy {
    ManagedOnly,
    SourceUserRc,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretEnvironmentReference {
    pub name: String,
    pub reference_id: String,
}

impl Debug for SecretEnvironmentReference {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretEnvironmentReference")
            .field("name", &self.name)
            .field("reference_id", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredEnvironmentOverlay {
    pub values: BTreeMap<String, String>,
    pub inherited_names: Vec<String>,
    pub secret_refs: Vec<SecretEnvironmentReference>,
}

impl Debug for StructuredEnvironmentOverlay {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StructuredEnvironmentOverlay")
            .field("value_names", &self.values.keys().collect::<Vec<_>>())
            .field("inherited_names", &self.inherited_names)
            .field(
                "secret_names",
                &self
                    .secret_refs
                    .iter()
                    .map(|reference| &reference.name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl StructuredEnvironmentOverlay {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let entry_count = self
            .values
            .len()
            .saturating_add(self.inherited_names.len())
            .saturating_add(self.secret_refs.len());
        if entry_count > MAX_ENVIRONMENT_NAMES {
            return invalid("environment overlay contains too many names");
        }

        let mut names = BTreeSet::new();
        let mut bytes = 0usize;
        for (name, value) in &self.values {
            validate_overlay_environment_name(name)?;
            if value.contains('\0') || value.len() > MAX_ENVIRONMENT_VALUE_BYTES {
                return invalid("environment values must be bounded and contain no NUL");
            }
            if !names.insert(name.as_str()) {
                return invalid("environment names must be unique across overlay sources");
            }
            bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
        }
        for name in &self.inherited_names {
            validate_overlay_environment_name(name)?;
            if !names.insert(name.as_str()) {
                return invalid("environment names must be unique across overlay sources");
            }
            bytes = bytes.saturating_add(name.len());
        }
        for secret in &self.secret_refs {
            validate_overlay_environment_name(&secret.name)?;
            if secret.reference_id.is_empty()
                || secret.reference_id.len() > MAX_SECRET_REFERENCE_BYTES
                || secret.reference_id.contains(['\0', '\n', '\r'])
            {
                return invalid("secret reference IDs must be nonempty, bounded single-line text");
            }
            if !names.insert(secret.name.as_str()) {
                return invalid("environment names must be unique across overlay sources");
            }
            bytes = bytes
                .saturating_add(secret.name.len())
                .saturating_add(secret.reference_id.len());
        }
        if bytes > MAX_ENVIRONMENT_OVERLAY_BYTES {
            return invalid("environment overlay exceeds its byte bound");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanTerminalEnsure {
    pub session_id: SessionId,
    pub client_submission_id: String,
    pub execution_context_revision: u64,
    pub profile: ExecutionProfile,
    pub shell_profile_id: String,
    pub terminal_size: TerminalSize,
    pub agl_env: StructuredEnvironmentOverlay,
    pub host_startup: HostStartupPolicy,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanTerminalCommandSubmit {
    pub session_id: SessionId,
    pub terminal_id: TerminalSessionId,
    pub client_submission_id: String,
    pub writer_lease_id: WriterLeaseId,
    pub expected_command_sequence: u64,
    pub expected_prompt_generation: u64,
    pub command: String,
}

impl Debug for HumanTerminalCommandSubmit {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HumanTerminalCommandSubmit")
            .field("session_id", &self.session_id)
            .field("terminal_id", &self.terminal_id)
            .field("client_submission_id", &self.client_submission_id)
            .field("writer_lease_present", &true)
            .field("expected_command_sequence", &self.expected_command_sequence)
            .field(
                "expected_prompt_generation",
                &self.expected_prompt_generation,
            )
            .field("command_bytes", &self.command.len())
            .finish()
    }
}

impl HumanTerminalCommandSubmit {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        validate_bounded_identifier(
            &self.client_submission_id,
            MAX_CLIENT_SUBMISSION_ID_BYTES,
            "client submission ID",
        )?;
        if self.command.is_empty()
            || self.command.len() > crate::MAX_HUMAN_COMMAND_BYTES
            || self.command.chars().any(|character| {
                let code = character as u32;
                (code <= 0x1f && character != '\n' && character != '\t')
                    || (0x7f..=0x9f).contains(&code)
            })
        {
            return invalid(
                "Human terminal command must be nonempty, bounded UTF-8 without unsafe controls",
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanTerminalCommandAccepted {
    pub terminal_id: TerminalSessionId,
    pub command_sequence: u64,
    pub output_after_sequence: u64,
}

impl HumanTerminalEnsure {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        validate_bounded_identifier(
            &self.client_submission_id,
            MAX_CLIENT_SUBMISSION_ID_BYTES,
            "client submission ID",
        )?;
        validate_bounded_identifier(
            &self.shell_profile_id,
            MAX_SHELL_PROFILE_ID_BYTES,
            "shell profile ID",
        )?;
        self.terminal_size.validate().map_err(|error| {
            ApplicationError::new(ApplicationErrorCode::InvalidArguments, error.to_string())
        })?;
        self.agl_env.validate()?;
        if self.profile == ExecutionProfile::Workspace
            && self.host_startup != HostStartupPolicy::ManagedOnly
        {
            return invalid("workspace terminals require managed-only shell startup");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellProfileView {
    pub profile_id: String,
    pub program: SanitizedDisplayPath,
    pub executable_digest: String,
    pub config_digest: String,
}

impl ShellProfileView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        validate_bounded_identifier(
            &self.profile_id,
            MAX_SHELL_PROFILE_ID_BYTES,
            "shell profile ID",
        )?;
        self.program.validate()?;
        validate_digest(&self.executable_digest, "shell executable digest")?;
        validate_digest(&self.config_digest, "shell configuration digest")?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalPromptState {
    Starting,
    Ready,
    CommandRunning,
    ForegroundProcess,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalWriterView {
    Unassigned,
    Owner,
    HumanTakeover,
    Revoked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalOwnerView {
    Human {
        session_id: SessionId,
    },
    MainAgent {
        session_id: SessionId,
    },
    Subagent {
        root_run_id: RunId,
        owner_run_id: RunId,
    },
    SessionPromoted {
        session_id: SessionId,
        previous_owner_run_id: RunId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSessionView {
    pub terminal_id: TerminalSessionId,
    pub execution_id: ExecutionId,
    pub owner: TerminalOwnerView,
    pub profile: ExecutionProfile,
    pub shell: ShellProfileView,
    pub workspace_root: SanitizedDisplayPath,
    pub cwd: SanitizedDisplayPath,
    pub initial_environment_digest: String,
    pub environment_names: Vec<String>,
    pub command_sequence: u64,
    pub prompt_generation: Option<u64>,
    pub prompt_state: TerminalPromptState,
    pub process_state: ExecutionState,
    pub exit: Option<ExecutionExit>,
    pub writer: TerminalWriterView,
    pub promoted: bool,
}

impl TerminalSessionView {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.shell.validate()?;
        self.workspace_root.validate()?;
        self.cwd.validate()?;
        validate_digest(
            &self.initial_environment_digest,
            "initial environment digest",
        )?;
        if self.environment_names.len() > MAX_ENVIRONMENT_NAMES {
            return invalid("terminal environment-name projection exceeds its count bound");
        }
        let mut names = BTreeSet::new();
        for name in &self.environment_names {
            validate_environment_name(name)?;
            if !names.insert(name) {
                return invalid("terminal environment-name projection contains duplicates");
            }
        }
        if self.profile == ExecutionProfile::Host
            && !matches!(self.owner, TerminalOwnerView::Human { .. })
        {
            return invalid("persistent host terminals are restricted to Human owners");
        }
        if self.promoted != matches!(self.owner, TerminalOwnerView::SessionPromoted { .. }) {
            return invalid("terminal promoted flag must match its lifecycle owner");
        }
        if self.process_state.is_live() && self.exit.is_some() {
            return invalid("a live terminal cannot carry a process exit outcome");
        }
        if matches!(self.prompt_state, TerminalPromptState::Ready)
            != self.prompt_generation.is_some()
        {
            return invalid(
                "terminal prompt generation must be present exactly for a trusted ready prompt",
            );
        }
        Ok(())
    }

    pub fn validate_for_session(&self, session_id: &SessionId) -> Result<(), ApplicationError> {
        self.validate()?;
        let projected_session_id = match &self.owner {
            TerminalOwnerView::Human { session_id }
            | TerminalOwnerView::MainAgent { session_id }
            | TerminalOwnerView::SessionPromoted { session_id, .. } => Some(session_id),
            TerminalOwnerView::Subagent { .. } => None,
        };
        if projected_session_id.is_some_and(|projected| projected != session_id) {
            return invalid("terminal owner belongs to a different session");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEnsureDisposition {
    Created,
    Reused,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalEnsured {
    pub terminal: TerminalSessionView,
    pub disposition: TerminalEnsureDisposition,
}

impl TerminalEnsured {
    pub fn validate_for_session(&self, session_id: &SessionId) -> Result<(), ApplicationError> {
        self.terminal.validate_for_session(session_id)
    }
}

fn validate_environment_name(name: &str) -> Result<(), ApplicationError> {
    let mut bytes = name.bytes();
    let valid = !name.is_empty()
        && name.len() <= MAX_ENVIRONMENT_NAME_BYTES
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return invalid("environment names must be bounded POSIX identifiers");
    }
    Ok(())
}

fn validate_overlay_environment_name(name: &str) -> Result<(), ApplicationError> {
    validate_environment_name(name)?;
    if matches!(
        name,
        "PATH" | "PWD" | "OLDPWD" | "HOME" | "SHELL" | "ENV" | "BASH_ENV" | "ZDOTDIR"
    ) || name.starts_with("AGL_INTERNAL_")
        || name.starts_with("AGL_SHELL_INTEGRATION_")
    {
        return invalid("environment overlay cannot replace daemon-owned names");
    }
    Ok(())
}

fn validate_bounded_identifier(
    value: &str,
    maximum_bytes: usize,
    label: &str,
) -> Result<(), ApplicationError> {
    if value.is_empty() || value.len() > maximum_bytes || value.contains(['\0', '\n', '\r']) {
        return invalid(format!("{label} must be nonempty bounded single-line text"));
    }
    Ok(())
}

fn validate_digest(value: &str, label: &str) -> Result<(), ApplicationError> {
    if value.is_empty()
        || value.len() > MAX_DIGEST_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return invalid(format!("{label} must be nonempty bounded ASCII text"));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T, ApplicationError> {
    Err(ApplicationError::new(
        ApplicationErrorCode::InvalidArguments,
        message,
    ))
}
