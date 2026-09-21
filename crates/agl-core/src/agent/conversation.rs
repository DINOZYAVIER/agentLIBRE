use crate::ConversationId;
use serde::{Deserialize, Serialize};

use super::{AgentPresentation, AgentRunSnapshot, ExactPackageRef};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationView {
    pub id: ConversationId,
    pub display_name: Option<String>,
    pub function: ExactPackageRef,
    pub reasoning: super::ReasoningSelection,
    pub presentation: AgentPresentation,
    pub created_at_ms: i64,
    pub last_active_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationBinding {
    pub view: ConversationView,
    pub snapshot: AgentRunSnapshot,
}
