use std::fmt;

pub type Result<T> = std::result::Result<T, SupervisorError>;

#[derive(Debug)]
pub enum SupervisorError {
    InvalidOptions(String),
    Store(agl_store::StoreError),
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
            Self::Store(error) => write!(formatter, "durable supervisor store failed: {error}"),
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

impl From<agl_store::StoreError> for SupervisorError {
    fn from(error: agl_store::StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<serde_json::Error> for SupervisorError {
    fn from(error: serde_json::Error) -> Self {
        Self::Driver(error.to_string())
    }
}
