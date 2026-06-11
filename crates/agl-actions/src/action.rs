use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum ModelAction {
    Answer(String),
    ToolCall(ToolCall),
    MalformedToolCall(MalformedToolCall),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MalformedToolCall {
    pub raw_json: String,
    pub classification: MalformedToolJsonKind,
    pub repair: Option<ToolJsonRepair>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MalformedToolJsonKind {
    MissingTerminator,
    Syntax,
    InvalidShape,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolJsonRepair {
    Succeeded {
        strategy: RepairStrategy,
        repaired_json: String,
        tool_call: ToolCall,
    },
    Failed {
        strategy: RepairStrategy,
        message: String,
    },
}

impl ToolJsonRepair {
    pub fn strategy(&self) -> RepairStrategy {
        match self {
            ToolJsonRepair::Succeeded { strategy, .. }
            | ToolJsonRepair::Failed { strategy, .. } => *strategy,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStrategy {
    None,
    AppendMissingBrace,
    AcceptMissingTerminator,
    UnescapeQuotedJson,
}

impl RepairStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            RepairStrategy::None => "none",
            RepairStrategy::AppendMissingBrace => "append_missing_brace",
            RepairStrategy::AcceptMissingTerminator => "accept_missing_terminator",
            RepairStrategy::UnescapeQuotedJson => "unescape_quoted_json",
        }
    }
}
