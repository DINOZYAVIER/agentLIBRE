use std::fmt::{self, Display, Formatter};

use agl_extension::{
    Extension, ExtensionBindings, ExtensionDefinition, ExtensionHost, StaticExtensionFactory,
};
use agl_kernel::{HostBindingId, HostBindingRequirement, ToolBinding, ToolId};

const API_MAJOR: u32 = 1;

#[derive(Debug)]
struct ScreenBindError;

impl Display for ScreenBindError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("host binding for Extension `host.screen` is absent or has the wrong type")
    }
}

impl std::error::Error for ScreenBindError {}

fn binding_id() -> HostBindingId {
    HostBindingId::new(super::screen::EXTENSION_ID)
        .expect("screen Extension ID is a valid host binding ID")
}

struct ScreenExtension;

impl Extension for ScreenExtension {
    type BindError = ScreenBindError;

    fn definition() -> ExtensionDefinition {
        ExtensionDefinition::from_descriptor(
            API_MAJOR,
            super::screen::declaration()
                .with_host_binding(HostBindingRequirement::new(binding_id(), API_MAJOR)),
        )
        .expect("screen Extension definition is valid")
    }

    fn bind(host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
        let handler = host
            .shared_tool_handler(&binding_id())
            .ok_or(ScreenBindError)?;
        Ok(ExtensionBindings::new(
            [ToolBinding::from_shared(
                ToolId::new(super::SCREEN_CAPTURE_TOOL_ID).expect("screen Tool ID is valid"),
                handler.clone(),
            )],
            [],
        ))
    }
}

pub fn screen_extension_factory() -> StaticExtensionFactory {
    StaticExtensionFactory::for_extension::<ScreenExtension>()
}
