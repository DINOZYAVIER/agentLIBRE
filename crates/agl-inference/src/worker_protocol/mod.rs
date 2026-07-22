mod error;
mod protocol;

pub use error::{Result, WorkerProtocolError, WorkerProtocolErrorCode};
pub use protocol::{
    ActiveOperationStatus, AllocationReceipt, ContextResourceId, DeviceKind, DeviceSnapshot,
    DeviceSnapshotEntry, Handshake, HandshakeRejected, HandshakeRejectionCode, HostCommand,
    InventorySnapshot, LiveContextInventoryEntry, LoadedModelInventoryEntry,
    MAX_CONTROL_DESCRIPTORS, MAX_CONTROL_FRAME_BYTES, MAX_DEVICE_DESCRIPTION_BYTES,
    MAX_DEVICE_SNAPSHOT_ENTRIES, MAX_PROTOCOL_LABEL_BYTES, MAX_SANDBOX_PATHS_PER_CLASS,
    MAX_SANDBOX_TOTAL_PATH_BYTES, MAX_SEALED_PAYLOAD_BYTES, MAX_WORKER_CONTEXTS,
    MAX_WORKER_FAILURE_MESSAGE_BYTES, MAX_WORKER_LOG_CODE_BYTES, MAX_WORKER_LOG_FIELD_KEY_BYTES,
    MAX_WORKER_LOG_FIELD_VALUE_BYTES, MAX_WORKER_LOG_FIELDS, MAX_WORKER_MODELS, ModelResourceId,
    OperationId, ProtocolLimits, Ready, SandboxConfiguration, SealedPayload, Shutdown,
    ShutdownComplete, ShutdownReason, WORKER_BINARY_NAME, WORKER_BUILD_ID,
    WORKER_DEVICE_LOST_EXIT_STATUS, WORKER_FRAME_VERSION, WORKER_PROTOCOL_ID, WorkerCapabilities,
    WorkerEvent, WorkerFailure, WorkerFailureCode, WorkerHealth, WorkerIdentity, WorkerLogField,
    WorkerLogLevel, WorkerLogRecord, WorkerOperationKind, WorkerStatusSnapshot,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
    DescriptorSet, HostCommandSender, HostControlChannel, MAX_WORKER_STDERR_EVIDENCE_BYTES,
    PacketSocket, ReceivedMessage, ReceivedPacket, SealedPayloadTransfer, WorkerCommandReceiver,
    WorkerControlChannel, WorkerEventReceiver, WorkerEventSender, WorkerExecutable, WorkerProcess,
    control_channel_pair,
};
