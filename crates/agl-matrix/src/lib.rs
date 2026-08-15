use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter, Write as _};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MatrixOutboxId(String);

impl MatrixOutboxId {
    pub fn new(value: impl Into<String>) -> Result<Self, MatrixMachineError> {
        let value = value.into();
        validate_bounded("outbox_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for MatrixOutboxId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MatrixOperationId(String);

impl MatrixOperationId {
    pub fn new(value: impl Into<String>) -> Result<Self, MatrixMachineError> {
        let value = value.into();
        validate_bounded("operation_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for MatrixOperationId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MatrixRevision(u64);

impl MatrixRevision {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: u64) -> Result<Self, MatrixMachineError> {
        if value == 0 {
            return Err(MatrixMachineError::InvalidValue {
                field: "revision",
                reason: "revision must be positive".to_owned(),
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Result<Self, MatrixMachineError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(MatrixMachineError::RevisionOverflow)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MatrixOutboxDraft {
    pub notify_ref: String,
    pub source_kind: String,
    pub source_id: String,
    pub dedupe_key: String,
    pub body: String,
}

impl MatrixOutboxDraft {
    pub fn new(
        notify_ref: impl Into<String>,
        source_kind: impl Into<String>,
        source_id: impl Into<String>,
        dedupe_key: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Self, MatrixMachineError> {
        let draft = Self {
            notify_ref: notify_ref.into(),
            source_kind: source_kind.into(),
            source_id: source_id.into(),
            dedupe_key: dedupe_key.into(),
            body: body.into(),
        };
        validate_bounded("notify_ref", &draft.notify_ref)?;
        validate_bounded("source_kind", &draft.source_kind)?;
        validate_bounded("source_id", &draft.source_id)?;
        validate_bounded("dedupe_key", &draft.dedupe_key)?;
        if draft.body.is_empty() || draft.body.len() > 1_048_576 {
            return Err(MatrixMachineError::InvalidValue {
                field: "body",
                reason: "body must be nonempty and at most 1 MiB".to_owned(),
            });
        }
        Ok(draft)
    }

    pub fn payload_fingerprint(&self) -> String {
        canonical_payload_fingerprint(self)
    }
}

pub fn canonical_payload_fingerprint(draft: &MatrixOutboxDraft) -> String {
    let mut hasher = Sha256::new();
    for field in [
        draft.notify_ref.as_bytes(),
        draft.source_kind.as_bytes(),
        draft.source_id.as_bytes(),
        draft.body.as_bytes(),
    ] {
        hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(field);
    }
    format!("sha256:{}", hex_digest(&hasher.finalize()))
}

pub fn stable_matrix_transaction_id(id: &MatrixOutboxId) -> String {
    let digest = Sha256::digest(id.as_str().as_bytes());
    format!("agl_{}", hex_digest(&digest))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MatrixOutboxState {
    Queued {
        not_before_ms: u64,
    },
    Delivering {
        lease_owner: String,
        lease_expires_at_ms: u64,
        attempt: u32,
    },
    Sent,
    Failed {
        error: String,
    },
}

impl MatrixOutboxState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued { .. } => "queued",
            Self::Delivering { .. } => "delivering",
            Self::Sent => "sent",
            Self::Failed { .. } => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Sent | Self::Failed { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MatrixOutboxRecord {
    pub id: MatrixOutboxId,
    pub draft: MatrixOutboxDraft,
    pub payload_fingerprint: String,
    pub transaction_id: String,
    pub state: MatrixOutboxState,
    pub revision: MatrixRevision,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub delivered_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MatrixDeliveryResult {
    Delivered,
    Retryable { not_before_ms: u64, error: String },
    Permanent { error: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MatrixCommand {
    Claim {
        lease_owner: String,
        now_ms: u64,
        lease_expires_at_ms: u64,
    },
    Complete {
        lease_owner: String,
        result: MatrixDeliveryResult,
    },
    RecoverExpired {
        now_ms: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatrixOutboxTransition {
    pub operation_id: MatrixOperationId,
    pub previous_state: MatrixOutboxState,
    pub new_state: MatrixOutboxState,
    pub previous_revision: MatrixRevision,
    pub new_revision: MatrixRevision,
}

#[derive(Clone, Debug)]
pub struct MatrixOutboxMachine {
    record: MatrixOutboxRecord,
    accepted: BTreeMap<MatrixOperationId, (MatrixCommand, MatrixOutboxTransition)>,
}

impl MatrixOutboxMachine {
    pub fn enqueue(id: MatrixOutboxId, draft: MatrixOutboxDraft, not_before_ms: u64) -> Self {
        let fingerprint = draft.payload_fingerprint();
        let transaction_id = stable_matrix_transaction_id(&id);
        Self {
            record: MatrixOutboxRecord {
                id,
                draft,
                payload_fingerprint: fingerprint,
                transaction_id,
                state: MatrixOutboxState::Queued { not_before_ms },
                revision: MatrixRevision::INITIAL,
                attempts: 0,
                last_error: None,
                created_at: String::new(),
                updated_at: String::new(),
                delivered_at: None,
            },
            accepted: BTreeMap::new(),
        }
    }

    pub fn restore(record: MatrixOutboxRecord) -> Result<Self, MatrixMachineError> {
        if record.payload_fingerprint != record.draft.payload_fingerprint() {
            return Err(MatrixMachineError::InvalidValue {
                field: "payload_fingerprint",
                reason: "stored fingerprint does not match payload".to_owned(),
            });
        }
        if record.transaction_id != stable_matrix_transaction_id(&record.id) {
            return Err(MatrixMachineError::InvalidValue {
                field: "transaction_id",
                reason: "stored transaction ID does not match outbox identity".to_owned(),
            });
        }
        Ok(Self {
            record,
            accepted: BTreeMap::new(),
        })
    }

    pub fn record(&self) -> &MatrixOutboxRecord {
        &self.record
    }

    pub fn exact_replay(
        &self,
        draft: &MatrixOutboxDraft,
    ) -> Result<MatrixOutboxRecord, MatrixEnqueueError> {
        let actual = draft.payload_fingerprint();
        if self.record.draft.dedupe_key == draft.dedupe_key
            && self.record.payload_fingerprint == actual
        {
            Ok(self.record.clone())
        } else {
            Err(MatrixEnqueueError::IdempotencyConflict {
                dedupe_key: draft.dedupe_key.clone(),
                expected_fingerprint: self.record.payload_fingerprint.clone(),
                actual_fingerprint: actual,
            })
        }
    }

    pub fn claim(
        &mut self,
        operation_id: MatrixOperationId,
        lease_owner: impl Into<String>,
        now_ms: u64,
        lease_expires_at_ms: u64,
    ) -> Result<MatrixOutboxTransition, MatrixMachineError> {
        let lease_owner = lease_owner.into();
        validate_bounded("lease_owner", &lease_owner)?;
        self.apply(
            operation_id,
            MatrixCommand::Claim {
                lease_owner,
                now_ms,
                lease_expires_at_ms,
            },
        )
    }

    pub fn complete(
        &mut self,
        operation_id: MatrixOperationId,
        lease_owner: impl Into<String>,
        result: MatrixDeliveryResult,
    ) -> Result<MatrixOutboxTransition, MatrixMachineError> {
        let lease_owner = lease_owner.into();
        validate_bounded("lease_owner", &lease_owner)?;
        self.apply(
            operation_id,
            MatrixCommand::Complete {
                lease_owner,
                result,
            },
        )
    }

    pub fn recover_expired(
        &mut self,
        operation_id: MatrixOperationId,
        now_ms: u64,
    ) -> Result<MatrixOutboxTransition, MatrixMachineError> {
        self.apply(operation_id, MatrixCommand::RecoverExpired { now_ms })
    }

    fn apply(
        &mut self,
        operation_id: MatrixOperationId,
        command: MatrixCommand,
    ) -> Result<MatrixOutboxTransition, MatrixMachineError> {
        if let Some((accepted_input, transition)) = self.accepted.get(&operation_id) {
            return if *accepted_input == command {
                Ok(transition.clone())
            } else {
                Err(MatrixMachineError::IdempotencyConflict { operation_id })
            };
        }
        if self.record.state.is_terminal() {
            return Err(MatrixMachineError::Terminal {
                state: self.record.state.clone(),
            });
        }

        let new_state = match (&self.record.state, &command) {
            (
                MatrixOutboxState::Queued { not_before_ms },
                MatrixCommand::Claim {
                    lease_owner,
                    now_ms,
                    lease_expires_at_ms,
                },
            ) => {
                if now_ms < not_before_ms {
                    return Err(MatrixMachineError::NotEligible {
                        not_before_ms: *not_before_ms,
                    });
                }
                if lease_expires_at_ms <= now_ms {
                    return Err(MatrixMachineError::InvalidValue {
                        field: "lease_expires_at_ms",
                        reason: "lease deadline must be after claim time".to_owned(),
                    });
                }
                let attempt = self
                    .record
                    .attempts
                    .checked_add(1)
                    .ok_or(MatrixMachineError::AttemptOverflow)?;
                MatrixOutboxState::Delivering {
                    lease_owner: lease_owner.clone(),
                    lease_expires_at_ms: *lease_expires_at_ms,
                    attempt,
                }
            }
            (
                MatrixOutboxState::Delivering {
                    lease_owner: current_owner,
                    ..
                },
                MatrixCommand::Complete {
                    lease_owner,
                    result,
                },
            ) => {
                if current_owner != lease_owner {
                    return Err(MatrixMachineError::LeaseMismatch);
                }
                match result {
                    MatrixDeliveryResult::Delivered => MatrixOutboxState::Sent,
                    MatrixDeliveryResult::Retryable {
                        not_before_ms,
                        error,
                    } => {
                        validate_error(error)?;
                        MatrixOutboxState::Queued {
                            not_before_ms: *not_before_ms,
                        }
                    }
                    MatrixDeliveryResult::Permanent { error } => {
                        validate_error(error)?;
                        MatrixOutboxState::Failed {
                            error: error.clone(),
                        }
                    }
                }
            }
            (
                MatrixOutboxState::Delivering {
                    lease_expires_at_ms,
                    ..
                },
                MatrixCommand::RecoverExpired { now_ms },
            ) => {
                if now_ms < lease_expires_at_ms {
                    return Err(MatrixMachineError::LeaseNotExpired {
                        lease_expires_at_ms: *lease_expires_at_ms,
                    });
                }
                MatrixOutboxState::Queued {
                    not_before_ms: *now_ms,
                }
            }
            _ => {
                return Err(MatrixMachineError::InvalidTransition {
                    state: self.record.state.clone(),
                });
            }
        };

        let new_revision = self.record.revision.next()?;
        let transition = MatrixOutboxTransition {
            operation_id: operation_id.clone(),
            previous_state: self.record.state.clone(),
            new_state,
            previous_revision: self.record.revision,
            new_revision,
        };
        self.record.state = transition.new_state.clone();
        self.record.revision = new_revision;
        if matches!(&command, MatrixCommand::Claim { .. }) {
            self.record.attempts = self
                .record
                .attempts
                .checked_add(1)
                .ok_or(MatrixMachineError::AttemptOverflow)?;
        }
        self.record.last_error = match &command {
            MatrixCommand::Complete {
                result: MatrixDeliveryResult::Retryable { error, .. },
                ..
            }
            | MatrixCommand::Complete {
                result: MatrixDeliveryResult::Permanent { error },
                ..
            } => Some(error.clone()),
            MatrixCommand::Complete {
                result: MatrixDeliveryResult::Delivered,
                ..
            } => None,
            _ => self.record.last_error.clone(),
        };
        self.accepted
            .insert(operation_id, (command, transition.clone()));
        Ok(transition)
    }
}

pub trait MatrixOutboxRepository: Send + Sync {
    fn enqueue(&self, draft: MatrixOutboxDraft) -> Result<MatrixOutboxRecord, MatrixError>;
    fn get(&self, id: &MatrixOutboxId) -> Result<Option<MatrixOutboxRecord>, MatrixError>;
    fn queued_page(&self, limit: usize) -> Result<Vec<MatrixOutboxRecord>, MatrixError>;
    fn queued(&self, now_ms: u64, limit: usize) -> Result<Vec<MatrixOutboxRecord>, MatrixError>;
    fn claim(
        &self,
        id: &MatrixOutboxId,
        operation_id: MatrixOperationId,
        lease_owner: &str,
        now_ms: u64,
        lease_expires_at_ms: u64,
    ) -> Result<MatrixOutboxRecord, MatrixError>;
    fn complete(
        &self,
        id: &MatrixOutboxId,
        operation_id: MatrixOperationId,
        lease_owner: &str,
        result: MatrixDeliveryResult,
    ) -> Result<MatrixOutboxRecord, MatrixError>;
    fn recover_expired(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<MatrixOutboxRecord>, MatrixError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum MatrixEnqueueError {
    #[error("matrix outbox dedupe key {dedupe_key} was reused with a different payload")]
    IdempotencyConflict {
        dedupe_key: String,
        expected_fingerprint: String,
        actual_fingerprint: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum MatrixMachineError {
    #[error("invalid Matrix outbox {field}: {reason}")]
    InvalidValue { field: &'static str, reason: String },
    #[error("Matrix operation {operation_id} was reused with different input")]
    IdempotencyConflict { operation_id: MatrixOperationId },
    #[error("Matrix outbox item is terminal in {state:?}")]
    Terminal { state: MatrixOutboxState },
    #[error("Matrix outbox item is not eligible before {not_before_ms}")]
    NotEligible { not_before_ms: u64 },
    #[error("Matrix outbox lease owner does not match")]
    LeaseMismatch,
    #[error("Matrix outbox lease is not expired before {lease_expires_at_ms}")]
    LeaseNotExpired { lease_expires_at_ms: u64 },
    #[error("invalid Matrix outbox transition from {state:?}")]
    InvalidTransition { state: MatrixOutboxState },
    #[error("Matrix outbox revision overflow")]
    RevisionOverflow,
    #[error("Matrix outbox attempt overflow")]
    AttemptOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum MatrixError {
    #[error(transparent)]
    Machine(#[from] MatrixMachineError),
    #[error(transparent)]
    Enqueue(#[from] MatrixEnqueueError),
    #[error("Matrix outbox item not found: {id}")]
    NotFound { id: String },
    #[error("Matrix outbox revision conflict for {id}")]
    RevisionConflict { id: String },
    #[error("Matrix outbox repository failed: {reason}")]
    Repository { reason: String },
}

fn validate_bounded(field: &'static str, value: &str) -> Result<(), MatrixMachineError> {
    if value.is_empty()
        || value.len() > 1024
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(MatrixMachineError::InvalidValue {
            field,
            reason: "must be nonempty bounded text without control or surrounding whitespace"
                .to_owned(),
        });
    }
    Ok(())
}

fn validate_error(error: &str) -> Result<(), MatrixMachineError> {
    if error.trim().is_empty() || error.len() > 16 * 1024 {
        return Err(MatrixMachineError::InvalidValue {
            field: "error",
            reason: "error must be nonblank and at most 16 KiB".to_owned(),
        });
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}
