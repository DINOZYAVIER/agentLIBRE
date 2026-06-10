use agl_actions::MalformedToolJsonKind;
use agl_events::{StopReasonEvent, ToolJsonMalformedKind};

use crate::StopReason;

pub(crate) fn malformed_kind(kind: MalformedToolJsonKind) -> ToolJsonMalformedKind {
    match kind {
        MalformedToolJsonKind::MissingTerminator => ToolJsonMalformedKind::MissingTerminator,
        MalformedToolJsonKind::Syntax => ToolJsonMalformedKind::Syntax,
        MalformedToolJsonKind::InvalidShape => ToolJsonMalformedKind::InvalidShape,
    }
}

pub(crate) fn stop_reason_event(reason: &StopReason) -> StopReasonEvent {
    match reason {
        StopReason::ToolJsonUnrepairable => StopReasonEvent::ToolJsonUnrepairable,
        StopReason::ToolLimitReached => StopReasonEvent::ToolLimitReached,
        StopReason::HiddenTool => StopReasonEvent::HiddenTool,
        StopReason::InvalidToolArguments => StopReasonEvent::InvalidToolArguments,
    }
}
