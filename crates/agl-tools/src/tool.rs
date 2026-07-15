use crate::ids::ToolId;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

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

pub(crate) fn parse_tool_args<T>(tool: &str, arguments: Value) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}
