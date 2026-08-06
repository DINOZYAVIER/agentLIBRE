use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use agl_kernel::{
    ExtensionDescriptor, ExtensionRegistration, ExtensionSource, ExtensionTrust, HookBinding,
    HookDeclaration, HookEvent, HookHandlerError, HookId, ToolBinding, ToolDispatchContext,
    ToolHandler, ToolHandlerFuture, ToolId, ToolResult, ToolRuntime,
};
use serde_json::Value;

#[derive(Clone, Copy)]
struct EmptyToolHandler;

impl ToolHandler for EmptyToolHandler {
    fn dispatch(&self, _context: ToolDispatchContext) -> ToolHandlerFuture<'_> {
        Box::pin(std::future::ready(Ok(ToolResult::new(serde_json::json!(
            {}
        )))))
    }
}

/// Test-only wiring point for complete kernel registration. The scenario owns
/// all declared and bound IDs; this adapter must only translate them to the
/// production ExtensionRegistration API.
pub struct ProductionRegistrationHarness {
    runtime: ToolRuntime,
}

impl ProductionRegistrationHarness {
    pub fn new() -> Self {
        Self {
            runtime: ToolRuntime::new(),
        }
    }

    pub fn snapshot_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&(
            self.runtime.catalog().extensions(),
            self.runtime
                .handler_ids()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            self.runtime
                .hook_handler_ids()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        ))
        .expect("registration snapshot serializes")
    }

    pub fn register(
        &mut self,
        extension_id: &str,
        declared_tools: &[&str],
        bound_tools: &[&str],
        declared_hooks: &[&str],
        bound_hooks: &[&str],
    ) -> Result<(), String> {
        let descriptor = declared_tools.iter().try_fold(
            ExtensionDescriptor::new(
                agl_kernel::ExtensionId::new(extension_id).map_err(|error| error.to_string())?,
                "Core test Extension",
                "1.0.0",
                ExtensionSource::TestFixture,
                ExtensionTrust::TrustedRegistered,
            )
            .map_err(|error| error.to_string())?,
            |descriptor, id| {
                crate::support::tool_declaration(id)
                    .validate()
                    .map(|_| descriptor.with_tool(crate::support::tool_declaration(id)))
                    .map_err(|error| error.to_string())
            },
        )?;
        let descriptor = declared_hooks
            .iter()
            .try_fold(descriptor, |descriptor, id| {
                let id = HookId::new(*id).map_err(|error| error.to_string())?;
                Ok::<_, String>(
                    descriptor.with_hook(HookDeclaration::new(id, HookEvent::ContextPrepare)),
                )
            })?;
        let tools = bound_tools
            .iter()
            .map(|id| {
                ToolId::new(*id)
                    .map(|id| ToolBinding::new(id, EmptyToolHandler))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let hooks = bound_hooks
            .iter()
            .map(|id| {
                HookId::new(*id)
                    .map(|id| {
                        HookBinding::new(id, |_input| {
                            Ok::<_, HookHandlerError>(serde_json::json!({}))
                        })
                    })
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.runtime
            .register_extension(
                ExtensionRegistration::new(descriptor, tools).with_hook_bindings(hooks),
            )
            .map_err(|error| error.to_string())
    }
}

/// Test-only wiring point for Hook schema checks. The counter belongs to the
/// scenario and observes the real handler invocation; the adapter must not
/// validate JSON or alter the handler output itself.
pub struct ProductionHookHarness {
    declaration: HookDeclaration,
    binding: HookBinding,
}

impl ProductionHookHarness {
    pub fn new(
        input_schema: Value,
        output_schema: Value,
        handler_output: Value,
        handler_calls: Arc<AtomicUsize>,
    ) -> Result<Self, String> {
        let id = HookId::new("example.hook:validate").map_err(|error| error.to_string())?;
        let declaration = HookDeclaration::new(id.clone(), HookEvent::ContextPrepare)
            .with_schemas(input_schema, output_schema);
        declaration.validate().map_err(|error| error.to_string())?;
        let binding = HookBinding::new(id, move |_input| {
            handler_calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, HookHandlerError>(handler_output.clone())
        });
        Ok(Self {
            declaration,
            binding,
        })
    }

    pub fn invoke(&self, payload: Value) -> Result<Value, String> {
        self.binding
            .invoke(&self.declaration, payload)
            .map_err(|error| error.to_string())
    }
}
