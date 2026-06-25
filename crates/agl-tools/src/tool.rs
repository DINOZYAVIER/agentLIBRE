use crate::ids::ToolId;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolInput {
    pub id: ToolId,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOutput {
    pub observation: String,
}

pub trait ToolHandler {
    fn dispatch(&self, input: ToolInput) -> anyhow::Result<ToolOutput>;
}
