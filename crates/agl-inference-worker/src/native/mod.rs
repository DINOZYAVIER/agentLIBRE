mod context_slot;
mod generation;
mod model;
mod runtime;

pub(crate) use agl_llama_cpp_sys as ffi;

#[cfg(test)]
pub(crate) use generation::NativeAbortTestProbe;

#[cfg(test)]
mod native_smoke;

pub use runtime::{
    LlamaCppDeviceInfo, LlamaCppDeviceKind, LlamaCppModelRuntime, llama_cpp_device_inventory,
    llama_cpp_inference_device_inventory,
};
