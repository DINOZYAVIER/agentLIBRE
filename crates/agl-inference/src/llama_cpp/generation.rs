use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};

use crate::InferenceFinishReason;

use super::ffi;

pub(crate) struct LlamaCppGenerationControl<'a> {
    signal: Option<NativeAbortSignal<'a>>,
}

struct NativeAbortSignal<'a> {
    cancellation: &'a AtomicBool,
    deadline: Option<Instant>,
}

impl NativeAbortSignal<'_> {
    fn is_cancelled(&self) -> bool {
        self.cancellation.load(Ordering::Acquire)
    }
}

impl<'a> LlamaCppGenerationControl<'a> {
    pub(crate) fn cancellable(cancellation: &'a AtomicBool) -> Self {
        Self {
            signal: Some(NativeAbortSignal {
                cancellation,
                deadline: None,
            }),
        }
    }

    pub(crate) fn cancellable_until(
        cancellation: &'a AtomicBool,
        deadline: Option<Instant>,
    ) -> Self {
        let mut control = Self::cancellable(cancellation);
        if let Some(signal) = control.signal.as_mut() {
            signal.deadline = deadline;
        }
        control
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.signal
            .as_ref()
            .is_some_and(NativeAbortSignal::is_cancelled)
    }

    pub(crate) fn deadline_exceeded(&self) -> bool {
        self.signal
            .as_ref()
            .and_then(|signal| signal.deadline)
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(crate) fn should_abort(&self) -> bool {
        self.is_cancelled() || self.deadline_exceeded()
    }

    pub(super) fn ensure_running(&self) -> Result<()> {
        if self.should_abort() {
            bail!(LlamaCppGenerationCancelled);
        }
        Ok(())
    }

    pub(super) fn install_abort_callback<'control>(
        &'control self,
        target_context: *mut c_void,
        draft_context: Option<*mut c_void>,
    ) -> NativeAbortGuard<'control> {
        let Some(signal) = self.signal.as_ref() else {
            return NativeAbortGuard::inactive();
        };
        let data = std::ptr::from_ref(signal).cast_mut().cast::<c_void>();
        unsafe {
            ffi::llama_set_abort_callback(target_context, Some(llama_abort_callback), data);
            if let Some(draft_context) = draft_context {
                ffi::llama_set_abort_callback(draft_context, Some(llama_abort_callback), data);
            }
        }
        NativeAbortGuard {
            target_context: Some(target_context),
            draft_context,
            callback_data: Some(signal),
            // Callback teardown must happen on the installing thread.
            _not_send: PhantomData,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LlamaCppGenerationCancelled;

impl fmt::Display for LlamaCppGenerationCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("llama.cpp generation cancelled")
    }
}

impl Error for LlamaCppGenerationCancelled {}

pub(crate) struct LlamaCppGenerationOutput {
    pub(crate) content: String,
    pub(crate) finish_reason: InferenceFinishReason,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
}

pub(super) struct NativeAbortGuard<'control> {
    target_context: Option<*mut c_void>,
    draft_context: Option<*mut c_void>,
    callback_data: Option<&'control NativeAbortSignal<'control>>,
    _not_send: PhantomData<Rc<()>>,
}

impl NativeAbortGuard<'_> {
    fn inactive() -> Self {
        Self {
            target_context: None,
            draft_context: None,
            callback_data: None,
            _not_send: PhantomData,
        }
    }
}

impl Drop for NativeAbortGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(context) = self.draft_context {
                ffi::llama_set_abort_callback(context, None, std::ptr::null_mut());
            }
            if let Some(context) = self.target_context {
                ffi::llama_set_abort_callback(context, None, std::ptr::null_mut());
            }
        }
        self.callback_data = None;
    }
}

unsafe extern "C" fn llama_abort_callback(data: *mut c_void) -> bool {
    if data.is_null() {
        return false;
    }
    // SAFETY: `data` points into the generation control borrowed by
    // `NativeAbortGuard`. The guard removes both callbacks before that control
    // can move or be dropped.
    let signal = unsafe { &*data.cast::<NativeAbortSignal>() };
    signal.is_cancelled()
        || signal
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn cancellation_control_observes_admitted_job_flag() {
        let cancelled = AtomicBool::new(false);
        let control = LlamaCppGenerationControl::cancellable(&cancelled);

        assert!(control.ensure_running().is_ok());
        cancelled.store(true, Ordering::Release);

        let error = control.ensure_running().unwrap_err();
        assert!(
            error
                .downcast_ref::<LlamaCppGenerationCancelled>()
                .is_some()
        );
    }

    #[test]
    fn native_abort_callback_reads_the_current_flag_value() {
        let cancelled = AtomicBool::new(false);
        let signal = NativeAbortSignal {
            cancellation: &cancelled,
            deadline: None,
        };
        let data = std::ptr::from_ref(&signal).cast_mut().cast::<c_void>();

        assert!(!unsafe { llama_abort_callback(data) });
        cancelled.store(true, Ordering::Release);
        assert!(unsafe { llama_abort_callback(data) });
    }

    #[test]
    fn native_abort_callback_observes_expired_deadline() {
        let cancelled = AtomicBool::new(false);
        let signal = NativeAbortSignal {
            cancellation: &cancelled,
            deadline: Some(Instant::now()),
        };
        let data = std::ptr::from_ref(&signal).cast_mut().cast::<c_void>();

        assert!(unsafe { llama_abort_callback(data) });
    }
}
