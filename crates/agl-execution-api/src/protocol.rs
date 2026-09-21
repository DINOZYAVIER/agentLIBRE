use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use agl_core::{AgentRunId, ConversationId, RequestId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::{Uuid, Version};

pub const EXECUTION_PROTOCOL_SCHEMA: &str = "agentlibre.execution.v1alpha";
pub const MAX_EXECUTION_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EXECUTION_OUTPUT_READ_BYTES: u32 = 1024 * 1024;

macro_rules! execution_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn parse(value: &str) -> Result<Self, ExecutionProtocolError> {
                let payload = value.strip_prefix($prefix).ok_or_else(|| {
                    ExecutionProtocolError::invalid(concat!("invalid ", stringify!($name)))
                })?;
                let uuid = Uuid::parse_str(payload).map_err(|_| {
                    ExecutionProtocolError::invalid(concat!("invalid ", stringify!($name)))
                })?;
                if uuid.get_version() != Some(Version::SortRand)
                    || payload != uuid.hyphenated().to_string()
                {
                    return Err(ExecutionProtocolError::invalid(concat!(
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
            type Err = ExecutionProtocolError;

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
execution_id!(TerminalId, "term_");

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
    PrivateInference {
        artifacts: Vec<String>,
        listen_socket: String,
        device_paths: Vec<String>,
        address_space_limit_bytes: u64,
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
    pub fn validate(&self) -> Result<(), ExecutionProtocolError> {
        if self.argv.is_empty() || self.argv.len() > 256 {
            return Err(ExecutionProtocolError::invalid(
                "argv must contain 1 to 256 values",
            ));
        }
        if self
            .argv
            .iter()
            .any(|value| value.len() > 32_768 || value.contains('\0'))
        {
            return Err(ExecutionProtocolError::invalid(
                "argv values must be bounded and contain no NUL",
            ));
        }
        if !std::path::Path::new(&self.argv[0]).is_absolute() {
            return Err(ExecutionProtocolError::invalid("argv[0] must be absolute"));
        }
        if self.cwd.is_empty()
            || self.cwd.len() > 32_768
            || self.cwd.contains('\0')
            || !std::path::Path::new(&self.cwd).is_absolute()
        {
            return Err(ExecutionProtocolError::invalid(
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
            return Err(ExecutionProtocolError::invalid(
                "environment is invalid or exceeds its bounds",
            ));
        }
        let indefinite_private_inference = self.timeout_ms == 0
            && matches!(self.isolation, ExecutionIsolation::PrivateInference { .. });
        if (!indefinite_private_inference && !(1..=3_600_000).contains(&self.timeout_ms))
            || !(1..=16 * 1024 * 1024).contains(&self.max_output_bytes)
        {
            return Err(ExecutionProtocolError::invalid(
                "timeout or output bound is outside the supported range",
            ));
        }
        match (self.io, self.terminal_size) {
            (ExecutionIo::Pipes, None) => {}
            (ExecutionIo::Pty, Some(size)) if valid_size(size) => {}
            _ => {
                return Err(ExecutionProtocolError::invalid(
                    "terminal_size must be present only for PTY execution",
                ));
            }
        }
        if let ExecutionIsolation::PrivateInference {
            artifacts,
            listen_socket,
            device_paths,
            address_space_limit_bytes,
        } = &self.isolation
            && (!matches!(self.owner, ExecutionOwner::Runtime { .. })
                || self.io != ExecutionIo::Pipes
                || artifacts.is_empty()
                || artifacts.len() > 33
                || *address_space_limit_bytes == 0
                || !valid_absolute_path(listen_socket)
                || artifacts.iter().any(|path| !valid_absolute_path(path))
                || device_paths.len() > 64
                || device_paths.iter().any(|path| !valid_absolute_path(path)))
        {
            return Err(ExecutionProtocolError::invalid(
                "private inference isolation is invalid",
            ));
        }
        if let ExecutionOwner::Runtime { component } = &self.owner
            && (component.is_empty()
                || component.len() > 128
                || component.contains(['\0', '\n', '\r']))
        {
            return Err(ExecutionProtocolError::invalid(
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
pub enum ExecutionSignal {
    Interrupt,
    Terminate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionCommand {
    Start {
        request: ExecutionStartRequest,
    },
    Inspect {
        execution_id: ExecutionId,
    },
    InspectTerminal {
        terminal_id: TerminalId,
    },
    Read {
        execution_id: ExecutionId,
        after: u64,
        max_bytes: u32,
    },
    Write {
        terminal_id: TerminalId,
        data: Vec<u8>,
    },
    Resize {
        terminal_id: TerminalId,
        size: TerminalSize,
    },
    Signal {
        execution_id: ExecutionId,
        signal: ExecutionSignal,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProtocolRequest {
    pub schema: String,
    pub request_id: RequestId,
    pub command: ExecutionCommand,
}

impl ExecutionProtocolRequest {
    pub fn new(command: ExecutionCommand) -> Self {
        Self {
            schema: EXECUTION_PROTOCOL_SCHEMA.to_owned(),
            request_id: RequestId::generate(),
            command,
        }
    }

    pub fn validate(&self) -> Result<(), ExecutionProtocolError> {
        if self.schema != EXECUTION_PROTOCOL_SCHEMA {
            return Err(ExecutionProtocolError::new(
                ExecutionProtocolErrorCode::SchemaMismatch,
                "unsupported execution protocol schema",
                false,
            ));
        }
        match &self.command {
            ExecutionCommand::Start { request } => request.validate()?,
            ExecutionCommand::Read { max_bytes, .. }
                if !(1..=MAX_EXECUTION_OUTPUT_READ_BYTES).contains(max_bytes) =>
            {
                return Err(ExecutionProtocolError::invalid(
                    "output read bound is outside the supported range",
                ));
            }
            ExecutionCommand::Write { data, .. }
                if data.is_empty() || data.len() > MAX_EXECUTION_OUTPUT_READ_BYTES as usize =>
            {
                return Err(ExecutionProtocolError::invalid(
                    "terminal input is empty or oversized",
                ));
            }
            ExecutionCommand::Resize { size, .. } if !valid_size(*size) => {
                return Err(ExecutionProtocolError::invalid("terminal size is invalid"));
            }
            _ => {}
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|_| ExecutionProtocolError::invalid("request is not encodable"))?;
        if bytes.len() > MAX_EXECUTION_FRAME_BYTES {
            return Err(ExecutionProtocolError::new(
                ExecutionProtocolErrorCode::FrameTooLarge,
                "execution request exceeds the frame bound",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Running,
    Exited,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOutcome {
    Exit { code: i32 },
    Signal { signal: i32 },
    TimedOut,
    Terminated,
    UnknownAfterServiceRestart,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStatus {
    pub execution_id: ExecutionId,
    pub terminal_id: Option<TerminalId>,
    pub owner: ExecutionOwner,
    pub state: ExecutionState,
    pub outcome: Option<ExecutionOutcome>,
    pub output_bytes: u64,
    pub output_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOutput {
    pub execution_id: ExecutionId,
    pub after: u64,
    pub next: u64,
    pub chunks: Vec<ExecutionOutputChunk>,
    pub eof: bool,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutputStream {
    Stdout,
    Stderr,
    Pty,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOutputChunk {
    pub stream: ExecutionOutputStream,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionResponse {
    Started { status: ExecutionStatus },
    Status { status: ExecutionStatus },
    Output { output: ExecutionOutput },
    Acknowledged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProtocolResponse {
    pub schema: String,
    pub request_id: RequestId,
    pub result: Result<ExecutionResponse, ExecutionProtocolError>,
}

impl ExecutionProtocolResponse {
    pub fn success(request_id: RequestId, response: ExecutionResponse) -> Self {
        Self {
            schema: EXECUTION_PROTOCOL_SCHEMA.to_owned(),
            request_id,
            result: Ok(response),
        }
    }

    pub fn error(request_id: RequestId, error: ExecutionProtocolError) -> Self {
        Self {
            schema: EXECUTION_PROTOCOL_SCHEMA.to_owned(),
            request_id,
            result: Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProtocolErrorCode {
    InvalidRequest,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
    SchemaMismatch,
    FrameTooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProtocolError {
    pub code: ExecutionProtocolErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ExecutionProtocolError {
    pub fn new(
        code: ExecutionProtocolErrorCode,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ExecutionProtocolErrorCode::InvalidRequest, message, false)
    }
}

impl fmt::Display for ExecutionProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExecutionProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_start_requests_are_strict() {
        let id = ExecutionId::generate();
        assert_eq!(id.to_string().parse::<ExecutionId>().unwrap(), id);
        assert!(ExecutionId::parse("exec_not-a-uuid").is_err());

        let request = ExecutionStartRequest {
            owner: ExecutionOwner::Runtime {
                component: "test".to_owned(),
            },
            argv: vec!["/usr/bin/printf".to_owned(), "ok".to_owned()],
            cwd: "/tmp".to_owned(),
            environment: BTreeMap::new(),
            clear_environment: false,
            io: ExecutionIo::Pipes,
            timeout_ms: 1000,
            max_output_bytes: 1024,
            terminal_size: None,
            isolation: ExecutionIsolation::Standard,
        };
        request.validate().unwrap();
    }
}
