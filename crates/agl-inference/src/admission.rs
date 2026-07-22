use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// A backend observation expressed in bytes at one bounded point in time.
///
/// The worker is not trusted to provide internally consistent memory numbers.
/// Callers must pass this value through [`validate_device_snapshot`] before it
/// participates in an admission decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceMemorySnapshot {
    pub physical_device_id: String,
    pub driver_id: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub observed_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceMemoryEnvelope {
    pub physical_device_id: String,
    pub minimum_total_bytes: u64,
    pub maximum_total_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotPolicy {
    pub maximum_age_ms: u64,
    pub maximum_future_skew_ms: u64,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            maximum_age_ms: 5_000,
            maximum_future_skew_ms: 1_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedDeviceSnapshot {
    physical_device_id: String,
    driver_id: String,
    total_bytes: u64,
    available_bytes: u64,
    observed_at_unix_ms: u64,
}

impl ValidatedDeviceSnapshot {
    pub fn physical_device_id(&self) -> &str {
        &self.physical_device_id
    }

    pub fn driver_id(&self) -> &str {
        &self.driver_id
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn available_bytes(&self) -> u64 {
        self.available_bytes
    }

    pub fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }

    pub fn external_pressure_bytes(&self) -> u64 {
        self.total_bytes - self.available_bytes
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidentReservations {
    /// Shared model/projector allocations, with each exact model generation
    /// counted once by the host ledger.
    pub model_bytes: u64,
    /// The sum of every independently resident context reservation.
    pub context_bytes: u64,
    /// Active generation/compute allocations which are released at the end of
    /// the exact admitted operation.
    pub transient_bytes: u64,
    /// Conservative resident uncertainty retained for the lifetime of the
    /// model/context generation rather than shrunk to a worker receipt.
    pub uncertainty_bytes: u64,
    /// Already committed allocations which have not yet appeared in the live
    /// device observation.
    pub pending_bytes: u64,
}

impl ResidentReservations {
    pub fn total_bytes(self) -> Result<u64, AdmissionError> {
        checked_sum(&[
            self.model_bytes,
            self.context_bytes,
            self.transient_bytes,
            self.uncertainty_bytes,
            self.pending_bytes,
        ])
        .ok_or(AdmissionError::ArithmeticOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationEstimate {
    pub model_bytes: u64,
    pub context_bytes: u64,
    pub transient_bytes: u64,
    pub uncertainty_bytes: u64,
}

impl AllocationEstimate {
    pub fn envelope_bytes(self) -> Result<u64, AdmissionError> {
        let total = checked_sum(&[
            self.model_bytes,
            self.context_bytes,
            self.transient_bytes,
            self.uncertainty_bytes,
        ])
        .ok_or(AdmissionError::ArithmeticOverflow)?;
        if total == 0 {
            return Err(AdmissionError::EstimateUnknown);
        }
        Ok(total)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationReceipt {
    pub model_bytes: u64,
    pub context_bytes: u64,
    pub transient_bytes: u64,
}

impl AllocationReceipt {
    pub fn validate_against(
        self,
        estimate: AllocationEstimate,
    ) -> Result<(), ReceiptValidationError> {
        let reported = checked_sum(&[self.model_bytes, self.context_bytes, self.transient_bytes])
            .ok_or(ReceiptValidationError::ArithmeticOverflow)?;
        let admitted = estimate
            .envelope_bytes()
            .map_err(|_| ReceiptValidationError::ArithmeticOverflow)?;
        if self.model_bytes > estimate.model_bytes
            || self.context_bytes > estimate.context_bytes
            || self.transient_bytes > estimate.transient_bytes
            || reported > admitted
        {
            return Err(ReceiptValidationError::EnvelopeExceeded {
                admitted_bytes: admitted,
                reported_bytes: reported,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionPolicy {
    pub reserve_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionGrant {
    pub allocation_envelope_bytes: u64,
    pub resident_reservation_bytes: u64,
    pub reserve_bytes: u64,
    pub available_bytes: u64,
    pub external_pressure_bytes: u64,
}

/// Stable host identity for one atomic pending/active reservation.
///
/// The value is allocated while the ledger lock is held and is never reused by
/// that ledger generation. It is deliberately unrelated to a worker PID or a
/// native pointer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReservationToken(u64);

impl ReservationToken {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservationRequest {
    pub model_key: String,
    pub context_key: String,
    pub estimate: AllocationEstimate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReservation {
    pub token: ReservationToken,
    pub model_key: String,
    pub context_key: String,
    pub estimate: AllocationEstimate,
    pub grant: AdmissionGrant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReservationStatus {
    pub context_key: String,
    pub model_key: String,
    pub reserved_bytes: u64,
    pub active_operations: u32,
    pub pinned: bool,
    pub last_used_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservationLedgerStatus {
    pub physical_device_id: String,
    pub resident: ResidentReservations,
    pub pending_reservations: usize,
    pub active_reservations: usize,
    pub contexts: Vec<ContextReservationStatus>,
}

#[derive(Clone, Debug)]
struct PendingReservationEntry {
    request: ReservationRequest,
    effective_estimate: AllocationEstimate,
    grant: AdmissionGrant,
}

#[derive(Clone, Debug)]
struct ActiveReservationEntry {
    context_key: String,
    transient_bytes: u64,
}

#[derive(Clone, Debug)]
struct ContextReservationEntry {
    model_key: String,
    context_bytes: u64,
    uncertainty_bytes: u64,
    active_operations: u32,
    pinned: bool,
    last_used_sequence: u64,
}

/// The host's one byte-accounting authority for a physical device generation.
///
/// `reserve` performs its snapshot check, resident calculation and pending
/// insertion in one mutable operation. [`SharedReservationLedger`] supplies
/// the process-local mutex used by concurrent callers; the cross-process
/// authority is the separate kernel-backed device lease.
#[derive(Debug)]
pub struct ReservationLedger {
    physical_device_id: String,
    policy: AdmissionPolicy,
    next_token: u64,
    use_sequence: u64,
    models: BTreeMap<String, u64>,
    contexts: BTreeMap<String, ContextReservationEntry>,
    pending: BTreeMap<ReservationToken, PendingReservationEntry>,
    active: BTreeMap<ReservationToken, ActiveReservationEntry>,
}

impl ReservationLedger {
    pub fn new(
        physical_device_id: impl Into<String>,
        policy: AdmissionPolicy,
    ) -> Result<Self, ReservationLedgerError> {
        let physical_device_id = physical_device_id.into();
        validate_identity("physical_device_id", &physical_device_id)
            .map_err(ReservationLedgerError::SnapshotInvalid)?;
        Ok(Self {
            physical_device_id,
            policy,
            next_token: 1,
            use_sequence: 0,
            models: BTreeMap::new(),
            contexts: BTreeMap::new(),
            pending: BTreeMap::new(),
            active: BTreeMap::new(),
        })
    }

    pub fn reserve(
        &mut self,
        snapshot: &ValidatedDeviceSnapshot,
        request: ReservationRequest,
    ) -> Result<PendingReservation, ReservationLedgerError> {
        validate_resource_key("model_key", &request.model_key)?;
        validate_resource_key("context_key", &request.context_key)?;
        if snapshot.physical_device_id() != self.physical_device_id {
            return Err(ReservationLedgerError::DeviceIdentityMismatch);
        }
        if let Some(context) = self.contexts.get(&request.context_key)
            && context.model_key != request.model_key
        {
            return Err(ReservationLedgerError::ContextModelMismatch);
        }
        if self.pending.values().any(|pending| {
            pending.request.model_key == request.model_key
                || pending.request.context_key == request.context_key
        }) {
            return Err(ReservationLedgerError::ResourcePending);
        }

        let effective_estimate = AllocationEstimate {
            model_bytes: if self.models.contains_key(&request.model_key) {
                0
            } else {
                request.estimate.model_bytes
            },
            context_bytes: if self.contexts.contains_key(&request.context_key) {
                0
            } else {
                request.estimate.context_bytes
            },
            transient_bytes: request.estimate.transient_bytes,
            uncertainty_bytes: request.estimate.uncertainty_bytes,
        };
        let resident = self.resident_reservations()?;
        let grant = admit_allocation(snapshot, resident, effective_estimate, self.policy)
            .map_err(ReservationLedgerError::Admission)?;
        let token = self.allocate_token()?;
        self.pending.insert(
            token,
            PendingReservationEntry {
                request: request.clone(),
                effective_estimate,
                grant,
            },
        );
        Ok(PendingReservation {
            token,
            model_key: request.model_key,
            context_key: request.context_key,
            estimate: effective_estimate,
            grant,
        })
    }

    /// Validates a raw device observation and reserves its allocation in one
    /// ledger operation.
    ///
    /// Keeping the raw snapshot gate adjacent to the mutation makes the
    /// fail-closed ordering explicit: invalid units, impossible availability,
    /// stale evidence, or identity mismatch cannot create a pending token
    /// which a caller could subsequently dispatch to a native worker.
    pub fn validate_and_reserve(
        &mut self,
        snapshot: DeviceMemorySnapshot,
        envelope: &DeviceMemoryEnvelope,
        snapshot_policy: SnapshotPolicy,
        now_unix_ms: u64,
        request: ReservationRequest,
    ) -> Result<PendingReservation, ReservationLedgerError> {
        let snapshot = validate_device_snapshot(snapshot, envelope, snapshot_policy, now_unix_ms)
            .map_err(ReservationLedgerError::SnapshotInvalid)?;
        self.reserve(&snapshot, request)
    }

    /// Reconcile the worker receipt and retain the conservative admitted
    /// envelope. The receipt can invalidate an estimate, but cannot shrink the
    /// reservation below what the host admitted.
    pub fn commit(
        &mut self,
        token: ReservationToken,
        receipt: AllocationReceipt,
    ) -> Result<(), ReservationLedgerError> {
        let pending = self
            .pending
            .get(&token)
            .cloned()
            .ok_or(ReservationLedgerError::ReservationNotPending)?;
        receipt
            .validate_against(pending.effective_estimate)
            .map_err(ReservationLedgerError::ReceiptInvalid)?;

        if let Some(bytes) = self.models.get(&pending.request.model_key)
            && *bytes != pending.request.estimate.model_bytes
        {
            return Err(ReservationLedgerError::ResidentEstimateChanged);
        }
        if let Some(context) = self.contexts.get(&pending.request.context_key)
            && (context.model_key != pending.request.model_key
                || context.context_bytes != pending.request.estimate.context_bytes)
        {
            return Err(ReservationLedgerError::ResidentEstimateChanged);
        }

        self.pending.remove(&token);
        if pending.effective_estimate.model_bytes > 0 {
            self.models.insert(
                pending.request.model_key.clone(),
                pending.request.estimate.model_bytes,
            );
        }
        self.use_sequence = self
            .use_sequence
            .checked_add(1)
            .ok_or(ReservationLedgerError::SequenceExhausted)?;
        let context = self
            .contexts
            .entry(pending.request.context_key.clone())
            .or_insert(ContextReservationEntry {
                model_key: pending.request.model_key,
                context_bytes: pending.request.estimate.context_bytes,
                uncertainty_bytes: pending.effective_estimate.uncertainty_bytes,
                active_operations: 0,
                pinned: false,
                last_used_sequence: self.use_sequence,
            });
        context.active_operations = context
            .active_operations
            .checked_add(1)
            .ok_or(ReservationLedgerError::ActiveCountExhausted)?;
        context.last_used_sequence = self.use_sequence;
        self.active.insert(
            token,
            ActiveReservationEntry {
                context_key: pending.request.context_key,
                transient_bytes: pending.effective_estimate.transient_bytes,
            },
        );
        Ok(())
    }

    pub fn abort_pending(&mut self, token: ReservationToken) -> Result<(), ReservationLedgerError> {
        self.pending
            .remove(&token)
            .map(|_| ())
            .ok_or(ReservationLedgerError::ReservationNotPending)
    }

    pub fn finish_active(&mut self, token: ReservationToken) -> Result<(), ReservationLedgerError> {
        let active = self
            .active
            .remove(&token)
            .ok_or(ReservationLedgerError::ReservationNotActive)?;
        self.use_sequence = self
            .use_sequence
            .checked_add(1)
            .ok_or(ReservationLedgerError::SequenceExhausted)?;
        let context = self
            .contexts
            .get_mut(&active.context_key)
            .ok_or(ReservationLedgerError::ContextMissing)?;
        context.active_operations = context
            .active_operations
            .checked_sub(1)
            .ok_or(ReservationLedgerError::ActiveCountUnderflow)?;
        context.last_used_sequence = self.use_sequence;
        Ok(())
    }

    pub fn set_context_pinned(
        &mut self,
        context_key: &str,
        pinned: bool,
    ) -> Result<(), ReservationLedgerError> {
        let context = self
            .contexts
            .get_mut(context_key)
            .ok_or(ReservationLedgerError::ContextMissing)?;
        context.pinned = pinned;
        Ok(())
    }

    pub fn idle_lru_context(&self) -> Option<&str> {
        self.contexts
            .iter()
            .filter(|(_, context)| context.active_operations == 0 && !context.pinned)
            .min_by_key(|(key, context)| (context.last_used_sequence, *key))
            .map(|(key, _)| key.as_str())
    }

    pub fn contains_model(&self, model_key: &str) -> bool {
        self.models.contains_key(model_key)
    }

    pub fn contains_context(&self, context_key: &str) -> bool {
        self.contexts.contains_key(context_key)
    }

    pub fn release_context(&mut self, context_key: &str) -> Result<(), ReservationLedgerError> {
        let context = self
            .contexts
            .get(context_key)
            .ok_or(ReservationLedgerError::ContextMissing)?;
        if context.active_operations > 0 || context.pinned {
            return Err(ReservationLedgerError::ContextBusy);
        }
        self.contexts.remove(context_key);
        Ok(())
    }

    pub fn release_model(&mut self, model_key: &str) -> Result<(), ReservationLedgerError> {
        if self
            .contexts
            .values()
            .any(|context| context.model_key == model_key)
        {
            return Err(ReservationLedgerError::ModelHasContexts);
        }
        self.models
            .remove(model_key)
            .map(|_| ())
            .ok_or(ReservationLedgerError::ModelMissing)
    }

    /// A worker generation owns every resident native allocation. Reaping it
    /// releases the complete ledger generation exactly once.
    pub fn release_worker_generation(&mut self) {
        self.models.clear();
        self.contexts.clear();
        self.pending.clear();
        self.active.clear();
    }

    pub fn status(&self) -> Result<ReservationLedgerStatus, ReservationLedgerError> {
        Ok(ReservationLedgerStatus {
            physical_device_id: self.physical_device_id.clone(),
            resident: self.resident_reservations()?,
            pending_reservations: self.pending.len(),
            active_reservations: self.active.len(),
            contexts: self
                .contexts
                .iter()
                .map(|(context_key, context)| ContextReservationStatus {
                    context_key: context_key.clone(),
                    model_key: context.model_key.clone(),
                    reserved_bytes: context
                        .context_bytes
                        .saturating_add(context.uncertainty_bytes),
                    active_operations: context.active_operations,
                    pinned: context.pinned,
                    last_used_sequence: context.last_used_sequence,
                })
                .collect(),
        })
    }

    fn resident_reservations(&self) -> Result<ResidentReservations, ReservationLedgerError> {
        let model_bytes = checked_sum(self.models.values().copied().collect::<Vec<_>>().as_slice())
            .ok_or(ReservationLedgerError::ArithmeticOverflow)?;
        let context_bytes = checked_sum(
            self.contexts
                .values()
                .map(|context| context.context_bytes)
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .ok_or(ReservationLedgerError::ArithmeticOverflow)?;
        let uncertainty_bytes = checked_sum(
            self.contexts
                .values()
                .map(|context| context.uncertainty_bytes)
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .ok_or(ReservationLedgerError::ArithmeticOverflow)?;
        let transient_bytes = checked_sum(
            self.active
                .values()
                .map(|active| active.transient_bytes)
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .ok_or(ReservationLedgerError::ArithmeticOverflow)?;
        let pending_bytes = checked_sum(
            self.pending
                .values()
                .map(|pending| pending.grant.allocation_envelope_bytes)
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .ok_or(ReservationLedgerError::ArithmeticOverflow)?;
        Ok(ResidentReservations {
            model_bytes,
            context_bytes,
            transient_bytes,
            uncertainty_bytes,
            pending_bytes,
        })
    }

    fn allocate_token(&mut self) -> Result<ReservationToken, ReservationLedgerError> {
        let token = ReservationToken(self.next_token);
        self.next_token = self
            .next_token
            .checked_add(1)
            .ok_or(ReservationLedgerError::TokenExhausted)?;
        Ok(token)
    }
}

#[derive(Clone, Debug)]
pub struct SharedReservationLedger(Arc<Mutex<ReservationLedger>>);

impl SharedReservationLedger {
    pub fn new(ledger: ReservationLedger) -> Self {
        Self(Arc::new(Mutex::new(ledger)))
    }

    pub fn with<T>(
        &self,
        operation: impl FnOnce(&mut ReservationLedger) -> Result<T, ReservationLedgerError>,
    ) -> Result<T, ReservationLedgerError> {
        let mut ledger = self
            .0
            .lock()
            .map_err(|_| ReservationLedgerError::LockPoisoned)?;
        operation(&mut ledger)
    }
}

pub fn validate_device_snapshot(
    snapshot: DeviceMemorySnapshot,
    envelope: &DeviceMemoryEnvelope,
    policy: SnapshotPolicy,
    now_unix_ms: u64,
) -> Result<ValidatedDeviceSnapshot, SnapshotValidationError> {
    validate_identity("physical_device_id", &snapshot.physical_device_id)?;
    validate_identity("driver_id", &snapshot.driver_id)?;
    validate_identity("expected physical_device_id", &envelope.physical_device_id)?;
    if snapshot.physical_device_id != envelope.physical_device_id {
        return Err(SnapshotValidationError::DeviceIdentityMismatch);
    }
    if envelope.minimum_total_bytes == 0
        || envelope.minimum_total_bytes > envelope.maximum_total_bytes
    {
        return Err(SnapshotValidationError::InvalidEnvelope);
    }
    if snapshot.total_bytes == 0 {
        return Err(SnapshotValidationError::ZeroTotal);
    }
    if snapshot.total_bytes < envelope.minimum_total_bytes
        || snapshot.total_bytes > envelope.maximum_total_bytes
    {
        return Err(SnapshotValidationError::TotalOutsideEnvelope {
            total_bytes: snapshot.total_bytes,
            minimum_bytes: envelope.minimum_total_bytes,
            maximum_bytes: envelope.maximum_total_bytes,
        });
    }
    if snapshot.available_bytes > snapshot.total_bytes {
        return Err(SnapshotValidationError::AvailableExceedsTotal {
            available_bytes: snapshot.available_bytes,
            total_bytes: snapshot.total_bytes,
        });
    }
    if snapshot.observed_at_unix_ms > now_unix_ms {
        let skew = snapshot.observed_at_unix_ms - now_unix_ms;
        if skew > policy.maximum_future_skew_ms {
            return Err(SnapshotValidationError::FromFuture { skew_ms: skew });
        }
    } else {
        let age = now_unix_ms - snapshot.observed_at_unix_ms;
        if age > policy.maximum_age_ms {
            return Err(SnapshotValidationError::Stale { age_ms: age });
        }
    }

    Ok(ValidatedDeviceSnapshot {
        physical_device_id: snapshot.physical_device_id,
        driver_id: snapshot.driver_id,
        total_bytes: snapshot.total_bytes,
        available_bytes: snapshot.available_bytes,
        observed_at_unix_ms: snapshot.observed_at_unix_ms,
    })
}

pub fn admit_allocation(
    snapshot: &ValidatedDeviceSnapshot,
    resident: ResidentReservations,
    estimate: AllocationEstimate,
    policy: AdmissionPolicy,
) -> Result<AdmissionGrant, AdmissionError> {
    let resident_bytes = resident.total_bytes()?;
    let allocation_bytes = estimate.envelope_bytes()?;
    let required_bytes = checked_sum(&[resident_bytes, allocation_bytes, policy.reserve_bytes])
        .ok_or(AdmissionError::ArithmeticOverflow)?;
    let immediately_available_required = allocation_bytes
        .checked_add(policy.reserve_bytes)
        .ok_or(AdmissionError::ArithmeticOverflow)?;
    if required_bytes > snapshot.total_bytes
        || immediately_available_required > snapshot.available_bytes
    {
        return Err(AdmissionError::CapacityExceeded {
            required_bytes,
            available_bytes: snapshot.available_bytes,
            reserved_bytes: resident_bytes,
            pressure_bytes: snapshot.external_pressure_bytes(),
        });
    }
    Ok(AdmissionGrant {
        allocation_envelope_bytes: allocation_bytes,
        resident_reservation_bytes: resident_bytes,
        reserve_bytes: policy.reserve_bytes,
        available_bytes: snapshot.available_bytes,
        external_pressure_bytes: snapshot.external_pressure_bytes(),
    })
}

pub fn mib(value: u64) -> Result<u64, SnapshotValidationError> {
    value
        .checked_mul(MIB)
        .ok_or(SnapshotValidationError::UnitConversionOverflow)
}

fn validate_identity(field: &'static str, value: &str) -> Result<(), SnapshotValidationError> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(SnapshotValidationError::InvalidIdentity { field });
    }
    Ok(())
}

fn validate_resource_key(field: &'static str, value: &str) -> Result<(), ReservationLedgerError> {
    if value.is_empty()
        || value.len() > 256
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ReservationLedgerError::InvalidResourceKey { field });
    }
    Ok(())
}

fn checked_sum(values: &[u64]) -> Option<u64> {
    values
        .iter()
        .try_fold(0_u64, |total, value| total.checked_add(*value))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotValidationError {
    InvalidIdentity {
        field: &'static str,
    },
    DeviceIdentityMismatch,
    InvalidEnvelope,
    ZeroTotal,
    TotalOutsideEnvelope {
        total_bytes: u64,
        minimum_bytes: u64,
        maximum_bytes: u64,
    },
    AvailableExceedsTotal {
        available_bytes: u64,
        total_bytes: u64,
    },
    Stale {
        age_ms: u64,
    },
    FromFuture {
        skew_ms: u64,
    },
    UnitConversionOverflow,
}

impl SnapshotValidationError {
    pub fn code(&self) -> &'static str {
        "device_snapshot_invalid"
    }
}

impl fmt::Display for SnapshotValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity { field } => write!(formatter, "invalid {field}"),
            Self::DeviceIdentityMismatch => {
                formatter.write_str("device snapshot identity does not match admission envelope")
            }
            Self::InvalidEnvelope => formatter.write_str("device memory envelope is invalid"),
            Self::ZeroTotal => formatter.write_str("device snapshot reports zero total bytes"),
            Self::TotalOutsideEnvelope {
                total_bytes,
                minimum_bytes,
                maximum_bytes,
            } => write!(
                formatter,
                "device total {total_bytes} is outside {minimum_bytes}..={maximum_bytes} bytes"
            ),
            Self::AvailableExceedsTotal {
                available_bytes,
                total_bytes,
            } => write!(
                formatter,
                "device available bytes {available_bytes} exceed total bytes {total_bytes}"
            ),
            Self::Stale { age_ms } => {
                write!(formatter, "device snapshot is stale by {age_ms} ms")
            }
            Self::FromFuture { skew_ms } => {
                write!(formatter, "device snapshot is {skew_ms} ms in the future")
            }
            Self::UnitConversionOverflow => {
                formatter.write_str("device memory unit conversion overflowed")
            }
        }
    }
}

impl std::error::Error for SnapshotValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    EstimateUnknown,
    ArithmeticOverflow,
    CapacityExceeded {
        required_bytes: u64,
        available_bytes: u64,
        reserved_bytes: u64,
        pressure_bytes: u64,
    },
}

impl AdmissionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EstimateUnknown => "resource_estimate_unknown",
            Self::ArithmeticOverflow => "resource_arithmetic_overflow",
            Self::CapacityExceeded { .. } => "accelerator_capacity_exceeded",
        }
    }
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EstimateUnknown => {
                formatter.write_str("inference allocation estimate is empty or unknown")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("inference resource arithmetic overflowed")
            }
            Self::CapacityExceeded {
                required_bytes,
                available_bytes,
                reserved_bytes,
                pressure_bytes,
            } => write!(
                formatter,
                "inference needs {required_bytes} bytes with {reserved_bytes} already reserved, but only {available_bytes} bytes are available under {pressure_bytes} bytes of device pressure"
            ),
        }
    }
}

impl std::error::Error for AdmissionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiptValidationError {
    ArithmeticOverflow,
    EnvelopeExceeded {
        admitted_bytes: u64,
        reported_bytes: u64,
    },
}

impl ReceiptValidationError {
    pub fn code(&self) -> &'static str {
        "resource_estimate_exceeded"
    }
}

impl fmt::Display for ReceiptValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArithmeticOverflow => {
                formatter.write_str("worker allocation receipt arithmetic overflowed")
            }
            Self::EnvelopeExceeded {
                admitted_bytes,
                reported_bytes,
            } => write!(
                formatter,
                "worker receipt violated an admitted component bound (reported total {reported_bytes} bytes; admitted total {admitted_bytes} bytes)"
            ),
        }
    }
}

impl std::error::Error for ReceiptValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReservationLedgerError {
    SnapshotInvalid(SnapshotValidationError),
    Admission(AdmissionError),
    ReceiptInvalid(ReceiptValidationError),
    InvalidResourceKey { field: &'static str },
    DeviceIdentityMismatch,
    ContextModelMismatch,
    ResourcePending,
    ReservationNotPending,
    ReservationNotActive,
    ResidentEstimateChanged,
    ContextMissing,
    ContextBusy,
    ModelMissing,
    ModelHasContexts,
    ArithmeticOverflow,
    TokenExhausted,
    SequenceExhausted,
    ActiveCountExhausted,
    ActiveCountUnderflow,
    LockPoisoned,
}

impl ReservationLedgerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SnapshotInvalid(error) => error.code(),
            Self::Admission(error) => error.code(),
            Self::ReceiptInvalid(error) => error.code(),
            Self::InvalidResourceKey { .. } => "resource_key_invalid",
            Self::DeviceIdentityMismatch => "device_identity_mismatch",
            Self::ContextModelMismatch => "context_model_mismatch",
            Self::ResourcePending => "resource_reservation_pending",
            Self::ReservationNotPending => "resource_reservation_not_pending",
            Self::ReservationNotActive => "resource_reservation_not_active",
            Self::ResidentEstimateChanged => "resource_estimate_changed",
            Self::ContextMissing => "context_reservation_missing",
            Self::ContextBusy => "context_reservation_busy",
            Self::ModelMissing => "model_reservation_missing",
            Self::ModelHasContexts => "model_reservation_has_contexts",
            Self::ArithmeticOverflow => "resource_arithmetic_overflow",
            Self::TokenExhausted => "resource_reservation_token_exhausted",
            Self::SequenceExhausted => "resource_use_sequence_exhausted",
            Self::ActiveCountExhausted => "resource_active_count_exhausted",
            Self::ActiveCountUnderflow => "resource_active_count_underflow",
            Self::LockPoisoned => "resource_ledger_unavailable",
        }
    }
}

impl fmt::Display for ReservationLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotInvalid(error) => write!(formatter, "{error}"),
            Self::Admission(error) => write!(formatter, "{error}"),
            Self::ReceiptInvalid(error) => write!(formatter, "{error}"),
            Self::InvalidResourceKey { field } => write!(formatter, "invalid {field}"),
            Self::DeviceIdentityMismatch => {
                formatter.write_str("snapshot belongs to another physical device")
            }
            Self::ContextModelMismatch => {
                formatter.write_str("context reservation belongs to another model")
            }
            Self::ResourcePending => {
                formatter.write_str("model or context already has a pending reservation")
            }
            Self::ReservationNotPending => formatter.write_str("reservation is not pending"),
            Self::ReservationNotActive => formatter.write_str("reservation is not active"),
            Self::ResidentEstimateChanged => {
                formatter.write_str("resident resource estimate changed within one generation")
            }
            Self::ContextMissing => formatter.write_str("context reservation does not exist"),
            Self::ContextBusy => formatter.write_str("context reservation is active or pinned"),
            Self::ModelMissing => formatter.write_str("model reservation does not exist"),
            Self::ModelHasContexts => {
                formatter.write_str("model reservation still has resident contexts")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("resource ledger arithmetic overflowed")
            }
            Self::TokenExhausted => formatter.write_str("reservation token space exhausted"),
            Self::SequenceExhausted => formatter.write_str("resource use sequence exhausted"),
            Self::ActiveCountExhausted => formatter.write_str("context active count exhausted"),
            Self::ActiveCountUnderflow => formatter.write_str("context active count underflowed"),
            Self::LockPoisoned => formatter.write_str("resource ledger lock is poisoned"),
        }
    }
}

impl std::error::Error for ReservationLedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SnapshotInvalid(error) => Some(error),
            Self::Admission(error) => Some(error),
            Self::ReceiptInvalid(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 10_000;

    fn incident_envelope() -> DeviceMemoryEnvelope {
        let total = mib(24_560).unwrap();
        DeviceMemoryEnvelope {
            physical_device_id: "pci:0000:03:00.0".to_string(),
            minimum_total_bytes: total,
            maximum_total_bytes: total,
        }
    }

    fn valid_snapshot(available_mib: u64) -> ValidatedDeviceSnapshot {
        let envelope = incident_envelope();
        validate_device_snapshot(
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:26.1".to_string(),
                total_bytes: envelope.maximum_total_bytes,
                available_bytes: mib(available_mib).unwrap(),
                observed_at_unix_ms: NOW,
            },
            &envelope,
            SnapshotPolicy::default(),
            NOW,
        )
        .unwrap()
    }

    #[test]
    fn impossible_available_value_fails_closed_without_clamping() {
        let envelope = incident_envelope();
        let error = validate_device_snapshot(
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:26.1".to_string(),
                total_bytes: envelope.maximum_total_bytes,
                available_bytes: u64::MAX,
                observed_at_unix_ms: NOW,
            },
            &envelope,
            SnapshotPolicy::default(),
            NOW,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SnapshotValidationError::AvailableExceedsTotal { .. }
        ));
        assert_eq!(error.code(), "device_snapshot_invalid");
        assert_eq!(
            mib(17_600_000_000_000_000),
            Err(SnapshotValidationError::UnitConversionOverflow)
        );
    }

    #[test]
    fn incident_profile_is_rejected_before_allocation() {
        let snapshot = valid_snapshot(24_560);
        let estimate = AllocationEstimate {
            model_bytes: mib(6_390).unwrap(),
            context_bytes: mib(18_788).unwrap(),
            transient_bytes: 0,
            uncertainty_bytes: 0,
        };
        let error = admit_allocation(
            &snapshot,
            ResidentReservations::default(),
            estimate,
            AdmissionPolicy { reserve_bytes: 0 },
        )
        .unwrap_err();

        assert!(matches!(error, AdmissionError::CapacityExceeded { .. }));
    }

    #[test]
    fn incident_arithmetic_is_exact_and_checked_in_bytes() {
        let model = mib(6_390).unwrap();
        let context = mib(18_788).unwrap();
        let two_context_demand = model
            .checked_add(context.checked_mul(2).unwrap())
            .expect("captured incident arithmetic fits u64 bytes");

        assert_eq!(two_context_demand, mib(43_966).unwrap());
        assert_eq!(two_context_demand / MIB, 43_966);
        assert_eq!(model.checked_add(context).unwrap(), mib(25_178).unwrap());
        assert!(two_context_demand > incident_envelope().maximum_total_bytes);
    }

    #[test]
    fn invalid_or_unit_mismatched_snapshot_creates_no_reservation_or_dispatch() {
        let envelope = incident_envelope();
        let mut ledger = ReservationLedger::new(
            envelope.physical_device_id.clone(),
            AdmissionPolicy { reserve_bytes: 0 },
        )
        .unwrap();
        let request = ReservationRequest {
            model_key: "incident-model".to_string(),
            context_key: "incident-context".to_string(),
            estimate: AllocationEstimate {
                model_bytes: mib(1).unwrap(),
                context_bytes: mib(1).unwrap(),
                transient_bytes: 0,
                uncertainty_bytes: 0,
            },
        };
        let snapshots = [
            // Captured live-test shape: a nonsensical multi-quadrillion
            // available value against a 24,560-MiB total.
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:26.1".to_string(),
                total_bytes: envelope.maximum_total_bytes,
                available_bytes: 17_600_000_000_000_000,
                observed_at_unix_ms: NOW,
            },
            // A producer accidentally supplied MiB scalar values to the byte
            // contract. The envelope catches the unit mismatch; it is never
            // normalized or multiplied heuristically.
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:26.1".to_string(),
                total_bytes: 24_560,
                available_bytes: 24_560,
                observed_at_unix_ms: NOW,
            },
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:26.1".to_string(),
                total_bytes: envelope.maximum_total_bytes,
                available_bytes: envelope.maximum_total_bytes,
                observed_at_unix_ms: NOW - SnapshotPolicy::default().maximum_age_ms - 1,
            },
        ];
        let mut native_dispatches = 0;

        for snapshot in snapshots {
            let result = ledger.validate_and_reserve(
                snapshot,
                &envelope,
                SnapshotPolicy::default(),
                NOW,
                request.clone(),
            );
            if result.is_ok() {
                native_dispatches += 1;
            }
            assert!(matches!(
                result,
                Err(ReservationLedgerError::SnapshotInvalid(_))
            ));
            let status = ledger.status().unwrap();
            assert_eq!(status.pending_reservations, 0);
            assert_eq!(status.active_reservations, 0);
            assert_eq!(status.resident, ResidentReservations::default());
        }

        assert_eq!(native_dispatches, 0);
        assert_eq!(
            mib(u64::MAX),
            Err(SnapshotValidationError::UnitConversionOverflow)
        );
    }

    #[test]
    fn a_second_incident_context_cannot_overcommit_the_device() {
        let snapshot = valid_snapshot(18_170);
        let estimate = AllocationEstimate {
            model_bytes: 0,
            context_bytes: mib(18_788).unwrap(),
            transient_bytes: 0,
            uncertainty_bytes: 0,
        };
        let resident = ResidentReservations {
            model_bytes: 0,
            context_bytes: 0,
            transient_bytes: 0,
            uncertainty_bytes: 0,
            pending_bytes: 0,
        };

        assert!(matches!(
            admit_allocation(
                &snapshot,
                resident,
                estimate,
                AdmissionPolicy { reserve_bytes: 0 }
            ),
            Err(AdmissionError::CapacityExceeded { .. })
        ));
    }

    #[test]
    fn reservations_and_operator_reserve_are_atomic_inputs() {
        let snapshot = valid_snapshot(16_000);
        let grant = admit_allocation(
            &snapshot,
            ResidentReservations {
                model_bytes: mib(2_000).unwrap(),
                context_bytes: mib(1_000).unwrap(),
                transient_bytes: 0,
                uncertainty_bytes: 0,
                pending_bytes: mib(500).unwrap(),
            },
            AllocationEstimate {
                model_bytes: mib(4_000).unwrap(),
                context_bytes: mib(4_000).unwrap(),
                transient_bytes: mib(1_000).unwrap(),
                uncertainty_bytes: mib(500).unwrap(),
            },
            AdmissionPolicy {
                reserve_bytes: mib(2_000).unwrap(),
            },
        )
        .unwrap();

        assert_eq!(grant.resident_reservation_bytes, mib(3_500).unwrap());
        assert_eq!(grant.allocation_envelope_bytes, mib(9_500).unwrap());
    }

    #[test]
    fn live_available_and_resident_budget_are_checked_without_double_counting() {
        let snapshot = valid_snapshot(10_000);
        let grant = admit_allocation(
            &snapshot,
            ResidentReservations {
                model_bytes: mib(12_000).unwrap(),
                context_bytes: 0,
                transient_bytes: 0,
                uncertainty_bytes: 0,
                pending_bytes: 0,
            },
            AllocationEstimate {
                model_bytes: 0,
                context_bytes: mib(4_000).unwrap(),
                transient_bytes: mib(1_000).unwrap(),
                uncertainty_bytes: 0,
            },
            AdmissionPolicy {
                reserve_bytes: mib(2_000).unwrap(),
            },
        )
        .unwrap();

        assert_eq!(grant.resident_reservation_bytes, mib(12_000).unwrap());
        assert_eq!(grant.allocation_envelope_bytes, mib(5_000).unwrap());
    }

    #[test]
    fn stale_future_and_overflowing_snapshots_are_rejected() {
        let envelope = incident_envelope();
        let base = DeviceMemorySnapshot {
            physical_device_id: envelope.physical_device_id.clone(),
            driver_id: "radv:26.1".to_string(),
            total_bytes: envelope.maximum_total_bytes,
            available_bytes: envelope.maximum_total_bytes,
            observed_at_unix_ms: 0,
        };
        assert!(matches!(
            validate_device_snapshot(base.clone(), &envelope, SnapshotPolicy::default(), NOW),
            Err(SnapshotValidationError::Stale { .. })
        ));
        assert!(matches!(
            validate_device_snapshot(
                DeviceMemorySnapshot {
                    observed_at_unix_ms: NOW + 2_000,
                    ..base
                },
                &envelope,
                SnapshotPolicy::default(),
                NOW
            ),
            Err(SnapshotValidationError::FromFuture { .. })
        ));
        assert_eq!(
            mib(u64::MAX),
            Err(SnapshotValidationError::UnitConversionOverflow)
        );
    }

    #[test]
    fn receipt_over_the_admitted_envelope_is_not_grown_opportunistically() {
        let estimate = AllocationEstimate {
            model_bytes: 10,
            context_bytes: 20,
            transient_bytes: 30,
            uncertainty_bytes: 10,
        };
        assert_eq!(
            AllocationReceipt {
                model_bytes: 10,
                context_bytes: 21,
                transient_bytes: 30,
            }
            .validate_against(estimate),
            Err(ReceiptValidationError::EnvelopeExceeded {
                admitted_bytes: 70,
                reported_bytes: 61,
            })
        );
    }

    #[test]
    fn checked_resource_arithmetic_rejects_wraparound() {
        let snapshot = valid_snapshot(24_560);
        let result = admit_allocation(
            &snapshot,
            ResidentReservations {
                model_bytes: u64::MAX,
                context_bytes: 1,
                transient_bytes: 0,
                uncertainty_bytes: 0,
                pending_bytes: 0,
            },
            AllocationEstimate {
                model_bytes: 1,
                context_bytes: 0,
                transient_bytes: 0,
                uncertainty_bytes: 0,
            },
            AdmissionPolicy { reserve_bytes: 0 },
        );
        assert_eq!(result, Err(AdmissionError::ArithmeticOverflow));
    }

    fn ledger(total_mib: u64, reserve_mib: u64) -> (ReservationLedger, ValidatedDeviceSnapshot) {
        let total = mib(total_mib).unwrap();
        let envelope = DeviceMemoryEnvelope {
            physical_device_id: "pci:0000:03:00.0".to_string(),
            minimum_total_bytes: total,
            maximum_total_bytes: total,
        };
        let snapshot = validate_device_snapshot(
            DeviceMemorySnapshot {
                physical_device_id: envelope.physical_device_id.clone(),
                driver_id: "radv:26.1".to_string(),
                total_bytes: total,
                available_bytes: total,
                observed_at_unix_ms: NOW,
            },
            &envelope,
            SnapshotPolicy::default(),
            NOW,
        )
        .unwrap();
        let ledger = ReservationLedger::new(
            envelope.physical_device_id,
            AdmissionPolicy {
                reserve_bytes: mib(reserve_mib).unwrap(),
            },
        )
        .unwrap();
        (ledger, snapshot)
    }

    fn request(
        model: &str,
        context: &str,
        model_mib: u64,
        context_mib: u64,
        transient_mib: u64,
    ) -> ReservationRequest {
        ReservationRequest {
            model_key: model.to_string(),
            context_key: context.to_string(),
            estimate: AllocationEstimate {
                model_bytes: mib(model_mib).unwrap(),
                context_bytes: mib(context_mib).unwrap(),
                transient_bytes: mib(transient_mib).unwrap(),
                uncertainty_bytes: 0,
            },
        }
    }

    #[test]
    fn pending_reservations_are_spent_atomically() {
        let (ledger, snapshot) = ledger(100, 0);
        let shared = SharedReservationLedger::new(ledger);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for index in 0..2 {
            let shared = shared.clone();
            let snapshot = snapshot.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                shared.with(|ledger| {
                    ledger.reserve(
                        &snapshot,
                        request(
                            &format!("model-{index}"),
                            &format!("context-{index}"),
                            30,
                            30,
                            0,
                        ),
                    )
                })
            }));
        }
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(ReservationLedgerError::Admission(
                        AdmissionError::CapacityExceeded { .. }
                    ))
                ))
                .count(),
            1
        );
        let status = shared.with(|ledger| ledger.status()).unwrap();
        assert_eq!(status.pending_reservations, 1);
        assert_eq!(status.resident.pending_bytes, mib(60).unwrap());
    }

    #[test]
    fn receipt_commit_retains_envelope_until_acknowledged_release() {
        let (mut ledger, snapshot) = ledger(32, 2);
        let pending = ledger
            .reserve(&snapshot, request("model", "context", 8, 12, 2))
            .unwrap();
        ledger
            .commit(
                pending.token,
                AllocationReceipt {
                    model_bytes: mib(7).unwrap(),
                    context_bytes: mib(11).unwrap(),
                    transient_bytes: mib(1).unwrap(),
                },
            )
            .unwrap();

        let status = ledger.status().unwrap();
        assert_eq!(status.resident.model_bytes, mib(8).unwrap());
        assert_eq!(status.resident.context_bytes, mib(12).unwrap());
        assert_eq!(status.resident.transient_bytes, mib(2).unwrap());
        assert_eq!(status.pending_reservations, 0);
        assert_eq!(status.active_reservations, 1);
        assert_eq!(ledger.idle_lru_context(), None);

        ledger.finish_active(pending.token).unwrap();
        assert_eq!(ledger.idle_lru_context(), Some("context"));
        assert_eq!(ledger.status().unwrap().resident.transient_bytes, 0);
        ledger.release_context("context").unwrap();
        ledger.release_model("model").unwrap();
        assert_eq!(
            ledger.status().unwrap().resident,
            ResidentReservations::default()
        );
    }

    #[test]
    fn over_envelope_receipt_never_grows_or_commits_resident_state() {
        let (mut ledger, snapshot) = ledger(32, 2);
        let pending = ledger
            .reserve(&snapshot, request("model", "context", 8, 12, 2))
            .unwrap();
        let error = ledger
            .commit(
                pending.token,
                AllocationReceipt {
                    model_bytes: mib(9).unwrap(),
                    context_bytes: mib(12).unwrap(),
                    transient_bytes: mib(2).unwrap(),
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ReservationLedgerError::ReceiptInvalid(ReceiptValidationError::EnvelopeExceeded { .. })
        ));
        let status = ledger.status().unwrap();
        assert_eq!(status.resident.model_bytes, 0);
        assert_eq!(status.resident.context_bytes, 0);
        assert_eq!(status.pending_reservations, 1);
        ledger.release_worker_generation();
        assert_eq!(
            ledger.status().unwrap().resident,
            ResidentReservations::default()
        );
    }

    #[test]
    fn eviction_is_idle_only_pinned_safe_and_deterministic() {
        let (mut ledger, snapshot) = ledger(64, 2);
        let first = ledger
            .reserve(&snapshot, request("model", "context-a", 8, 4, 1))
            .unwrap();
        ledger
            .commit(
                first.token,
                AllocationReceipt {
                    model_bytes: mib(8).unwrap(),
                    context_bytes: mib(4).unwrap(),
                    transient_bytes: mib(1).unwrap(),
                },
            )
            .unwrap();
        ledger.finish_active(first.token).unwrap();

        let second = ledger
            .reserve(&snapshot, request("model", "context-b", 8, 4, 1))
            .unwrap();
        ledger
            .commit(
                second.token,
                AllocationReceipt {
                    model_bytes: 0,
                    context_bytes: mib(4).unwrap(),
                    transient_bytes: mib(1).unwrap(),
                },
            )
            .unwrap();
        ledger.finish_active(second.token).unwrap();

        assert_eq!(ledger.idle_lru_context(), Some("context-a"));
        ledger.set_context_pinned("context-a", true).unwrap();
        assert_eq!(ledger.idle_lru_context(), Some("context-b"));
        assert_eq!(
            ledger.release_context("context-a"),
            Err(ReservationLedgerError::ContextBusy)
        );
    }

    #[test]
    fn worker_generation_release_is_complete_and_idempotent() {
        let (mut ledger, snapshot) = ledger(32, 2);
        let pending = ledger
            .reserve(&snapshot, request("model", "context", 8, 12, 2))
            .unwrap();
        ledger
            .commit(
                pending.token,
                AllocationReceipt {
                    model_bytes: mib(8).unwrap(),
                    context_bytes: mib(12).unwrap(),
                    transient_bytes: mib(2).unwrap(),
                },
            )
            .unwrap();
        ledger.release_worker_generation();
        ledger.release_worker_generation();
        let status = ledger.status().unwrap();
        assert_eq!(status.resident, ResidentReservations::default());
        assert_eq!(status.pending_reservations, 0);
        assert_eq!(status.active_reservations, 0);
    }
}
