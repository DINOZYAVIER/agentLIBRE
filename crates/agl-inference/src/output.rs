use agl_ids::AttemptId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InferenceOutputEvent {
    TextDelta {
        attempt_id: AttemptId,
        sequence: u64,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputDelivery {
    Delivered,
    Lagged,
    Closed,
}

pub trait InferenceOutputSink: Send + Sync + 'static {
    fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopInferenceOutputSink;

impl InferenceOutputSink for NoopInferenceOutputSink {
    fn try_emit(&self, _event: InferenceOutputEvent) -> OutputDelivery {
        OutputDelivery::Delivered
    }
}
