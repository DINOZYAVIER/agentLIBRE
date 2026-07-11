use std::fmt;

use crate::InferenceFinishReason;

#[derive(Debug)]
pub struct RuntimeOperation<T> {
    pub value: T,
    pub log: String,
}

impl<T> RuntimeOperation<T> {
    pub fn new(value: T, log: impl Into<String>) -> Self {
        Self {
            value,
            log: log.into(),
        }
    }

    pub fn without_log(value: T) -> Self {
        Self::new(value, String::new())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFailure {
    message: String,
    log: String,
}

impl RuntimeFailure {
    pub fn new(message: impl Into<String>, log: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            log: log.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn log(&self) -> &str {
        &self.log
    }
}

impl fmt::Display for RuntimeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeFailure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelGeneration {
    pub content: String,
    pub finish_reason: InferenceFinishReason,
    pub selected_device: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
