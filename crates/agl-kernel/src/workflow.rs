use agl_extension::WorkflowEventId;
use serde::{Deserialize, Serialize};

pub const TOOL_OBSERVATION_APPEND_EVENT_ID: &str = "agl:tool_observation.append";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelWorkflowEvent {
    ToolObservationAppend,
}

impl KernelWorkflowEvent {
    pub fn parse(id: &WorkflowEventId) -> Option<Self> {
        match id.as_str() {
            TOOL_OBSERVATION_APPEND_EVENT_ID => Some(Self::ToolObservationAppend),
            _ => None,
        }
    }

    pub fn id(self) -> WorkflowEventId {
        let id = match self {
            Self::ToolObservationAppend => TOOL_OBSERVATION_APPEND_EVENT_ID,
        };
        WorkflowEventId::new(id).expect("kernel workflow event IDs are valid")
    }
}
