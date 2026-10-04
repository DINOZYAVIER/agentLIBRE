use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::{AgentRunId, ConversationId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::{Uuid, Version};

pub const MAX_EXECUTION_OUTPUT_READ_BYTES: u32 = 1024 * 1024;

macro_rules! execution_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn parse(value: &str) -> Result<Self, ExecutionError> {
                let payload = value.strip_prefix($prefix).ok_or_else(|| {
                    ExecutionError::invalid(concat!("invalid ", stringify!($name)))
                })?;
                let uuid = Uuid::parse_str(payload)
                    .map_err(|_| ExecutionError::invalid(concat!("invalid ", stringify!($name))))?;
                if uuid.get_version() != Some(Version::SortRand)
                    || payload != uuid.hyphenated().to_string()
                {
                    return Err(ExecutionError::invalid(concat!(
                        "invalid ",
                        stringify!($name)
                    )));
                }
                Ok(Self(uuid))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}{}", $prefix, self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = ExecutionError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

execution_id!(ExecutionId, "exec_");
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOwner {
    Agent {
        conversation_id: ConversationId,
        run_id: AgentRunId,
    },
    Runtime {
        component: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionIo {
    Pipes,
    Pty,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionIsolation {
    #[default]
    Standard,
    /// Linux/Nix planner execution using an independent read-only reflink view.
    ReadOnlyWorkspace {
        workspace_root: String,
        output_directories: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub rows: u16,
    pub columns: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            columns: 80,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStartRequest {
    pub owner: ExecutionOwner,
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub clear_environment: bool,
    pub io: ExecutionIo,
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
    pub terminal_size: Option<TerminalSize>,
    #[serde(default)]
    pub isolation: ExecutionIsolation,
}

impl ExecutionStartRequest {
    pub fn validate(&self) -> Result<(), ExecutionError> {
        if self.argv.is_empty() || self.argv.len() > 256 {
            return Err(ExecutionError::invalid("argv must contain 1 to 256 values"));
        }
        if self
            .argv
            .iter()
            .any(|value| value.len() > 32_768 || value.contains('\0'))
        {
            return Err(ExecutionError::invalid(
                "argv values must be bounded and contain no NUL",
            ));
        }
        if !std::path::Path::new(&self.argv[0]).is_absolute() {
            return Err(ExecutionError::invalid("argv[0] must be absolute"));
        }
        if self.cwd.is_empty()
            || self.cwd.len() > 32_768
            || self.cwd.contains('\0')
            || !std::path::Path::new(&self.cwd).is_absolute()
        {
            return Err(ExecutionError::invalid(
                "cwd must be one bounded absolute path",
            ));
        }
        if self.environment.len() > 512
            || self.environment.iter().any(|(name, value)| {
                name.is_empty()
                    || name.len() > 256
                    || name.contains(['=', '\0'])
                    || value.len() > 32_768
                    || value.contains('\0')
            })
        {
            return Err(ExecutionError::invalid(
                "environment is invalid or exceeds its bounds",
            ));
        }
        if self.timeout_ms > 3_600_000 || self.max_output_bytes == 0 {
            return Err(ExecutionError::invalid("invalid timeout/output bound"));
        }
        match (self.io, self.terminal_size) {
            (ExecutionIo::Pipes, None) => {}
            (ExecutionIo::Pty, Some(size)) if valid_size(size) => {}
            _ => {
                return Err(ExecutionError::invalid(
                    "terminal_size must be present only for PTY execution",
                ));
            }
        }
        if let ExecutionIsolation::ReadOnlyWorkspace {
            workspace_root,
            output_directories,
        } = &self.isolation
            && (!matches!(self.owner, ExecutionOwner::Agent { .. })
                || self.io != ExecutionIo::Pipes
                || !valid_absolute_path(workspace_root)
                || output_directories.iter().any(|directory| {
                    directory.contains('\0')
                        || crate::implementation_plan::WorkspacePath::new(directory.clone())
                            .is_err()
                        || std::path::Path::new(directory) == std::path::Path::new(".")
                        || std::path::Path::new(directory)
                            == std::path::Path::new("agentLIBRE.toml")
                }))
        {
            return Err(ExecutionError::invalid(
                "read-only workspace isolation requires an Agent owner, pipes, an absolute workspace root, and contained output directory paths",
            ));
        }
        if let ExecutionOwner::Runtime { component } = &self.owner
            && (component.is_empty()
                || component.len() > 128
                || component.contains(['\0', '\n', '\r']))
        {
            return Err(ExecutionError::invalid(
                "runtime component identity is invalid",
            ));
        }
        Ok(())
    }
}

fn valid_absolute_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32_768
        && !value.contains('\0')
        && std::path::Path::new(value).is_absolute()
}

fn valid_size(size: TerminalSize) -> bool {
    (1..=1000).contains(&size.rows) && (1..=1000).contains(&size.columns)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Running,
    #[serde(rename = "finished")]
    Exited,
    #[serde(rename = "unknown")]
    OutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOutcome {
    Exit { code: i32 },
    Signal { signal: i32 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStatus {
    pub execution_id: ExecutionId,
    #[serde(rename = "status")]
    pub state: ExecutionState,
    pub outcome: Option<ExecutionOutcome>,
    pub stopped_by: Option<ExecutionStopCause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<ExecutionStreamStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<ExecutionStreamStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<ExecutionStreamStatus>,
    pub outputs_removed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStopCause {
    Stop,
    Timeout,
    RunCancel,
    TmuxLost,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionStreamStatus {
    pub bytes: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOutput {
    pub execution_id: ExecutionId,
    pub stream: ExecutionOutputStream,
    pub offset: u64,
    pub next: u64,
    pub data: Vec<u8>,
    pub eof: bool,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutputStream {
    Stdout,
    Stderr,
    #[serde(rename = "terminal")]
    Pty,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorKind {
    InvalidRequest,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionError {
    pub code: ExecutionErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ExecutionErrorKind::InvalidRequest, message, false)
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExecutionError {}

#[derive(Clone, Debug)]
pub enum ExecutionAction {
    Start(ExecutionStartRequest),
    Inspect(ExecutionId),
    Read {
        execution_id: ExecutionId,
        stream: ExecutionOutputStream,
        offset: u64,
        max_bytes: u32,
    },
    Wait {
        execution_id: ExecutionId,
        wait_timeout_ms: u64,
    },
    Write {
        execution_id: ExecutionId,
        data: Vec<u8>,
    },
    Resize {
        execution_id: ExecutionId,
        size: TerminalSize,
    },
    Interrupt(ExecutionId),
    Stop(ExecutionId),
    Cleanup(ExecutionId),
}
#[derive(Clone, Debug)]
pub enum ExecutionResult {
    Status(ExecutionStatus),
    Output(ExecutionOutput),
    Acknowledged,
}
