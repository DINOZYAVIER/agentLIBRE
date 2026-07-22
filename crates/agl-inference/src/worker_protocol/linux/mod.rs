mod payload;
mod process;
mod socket;

pub use payload::SealedPayloadTransfer;
pub use process::{MAX_WORKER_STDERR_EVIDENCE_BYTES, WorkerExecutable, WorkerProcess};
pub use socket::{
    DescriptorSet, HostCommandSender, HostControlChannel, PacketSocket, ReceivedMessage,
    ReceivedPacket, WorkerCommandReceiver, WorkerControlChannel, WorkerEventReceiver,
    WorkerEventSender, control_channel_pair,
};

const INHERITED_CONTROL_FD_ENV: &str = "AGL_INFERENCE_WORKER_CONTROL_FD";
const INHERITED_PARENT_PID_ENV: &str = "AGL_INFERENCE_WORKER_PARENT_PID";
