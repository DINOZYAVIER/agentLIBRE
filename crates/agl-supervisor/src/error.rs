use std::fmt;

pub type Result<T> = std::result::Result<T, SupervisorError>;

#[derive(Debug)]
pub enum SupervisorError {
    InvalidOptions(String),
    Repository(agl_kernel::RunRepositoryError),
    Driver(String),
    CommandQueueFull,
    Unavailable,
    SubscriberOverflow { last_sequence: u64 },
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions(message) => {
                write!(formatter, "invalid supervisor options: {message}")
            }
            Self::Repository(error) => {
                write!(formatter, "durable supervisor repository failed: {error}")
            }
            Self::Driver(message) => write!(formatter, "durable run driver failed: {message}"),
            Self::CommandQueueFull => formatter.write_str("supervisor command queue is full"),
            Self::Unavailable => formatter.write_str("supervisor coordinator is unavailable"),
            Self::SubscriberOverflow { last_sequence } => write!(
                formatter,
                "run event subscriber overflowed after sequence {last_sequence}"
            ),
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<agl_kernel::RunRepositoryError> for SupervisorError {
    fn from(error: agl_kernel::RunRepositoryError) -> Self {
        Self::Repository(error)
    }
}

impl From<serde_json::Error> for SupervisorError {
    fn from(error: serde_json::Error) -> Self {
        Self::Driver(error.to_string())
    }
}
