#![allow(dead_code)]

use agl_kernel::{
    ExtensionDescriptor, ExtensionId, ExtensionSource, ExtensionTrust, HookDeclaration, HookEvent,
    HookId, OperationKind, ToolDeclaration, ToolId,
};
use serde_json::{Value, json};

pub fn hook_id(value: &str) -> HookId {
    HookId::new(value).expect("test HookId is valid")
}

pub fn tool_id(value: &str) -> ToolId {
    ToolId::new(value).expect("test ToolId is valid")
}

pub fn extension_id(value: &str) -> ExtensionId {
    ExtensionId::new(value).expect("test ExtensionId is valid")
}

pub fn empty_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false
    })
}

pub fn tool_declaration(id: &str) -> ToolDeclaration {
    ToolDeclaration::new(
        tool_id(id),
        "Core test Tool",
        empty_schema(),
        OperationKind::Read,
    )
    .expect("test Tool declaration is valid")
}

pub fn extension_with_tool(extension: &str, tool: &str) -> ExtensionDescriptor {
    ExtensionDescriptor::new(
        extension_id(extension),
        "Core test Extension",
        "1.0.0",
        ExtensionSource::TestFixture,
        ExtensionTrust::TrustedRegistered,
    )
    .expect("test Extension descriptor is valid")
    .with_tool(tool_declaration(tool))
}

pub fn hook_declaration(id: &str, event: HookEvent) -> HookDeclaration {
    HookDeclaration::new(hook_id(id), event)
}
