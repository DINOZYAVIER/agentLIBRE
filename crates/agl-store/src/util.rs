use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Result, StoreError};

pub(crate) fn validate_non_empty_list(values: &[String], field: &'static str) -> Result<()> {
    if values.is_empty() {
        return Err(StoreError::InvalidValue {
            field,
            value: "[]".to_string(),
            reason: "list cannot be empty",
        });
    }
    for value in values {
        validate_non_blank(value, field)?;
    }
    Ok(())
}

pub(crate) fn validate_non_blank(value: &str, field: &'static str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StoreError::InvalidValue {
            field,
            value: value.to_string(),
            reason: match field {
                "namespace" => "namespace cannot be blank",
                "key" => "key cannot be blank",
                "fingerprint" => "fingerprint cannot be blank",
                _ => "value cannot be blank",
            },
        });
    }
    Ok(())
}

pub(crate) fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

pub(crate) fn store_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}
