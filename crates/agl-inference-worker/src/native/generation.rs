use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Instant;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::{Arc, Condvar, Mutex, OnceLock};
#[cfg(test)]
use std::time::Duration;

use anyhow::{Result, bail};

use agl_inference::{InferenceCancellation, InferenceFinishReason};

use super::ffi;

pub(crate) struct LlamaCppGenerationControl<'a> {
    signal: Option<NativeAbortSignal<'a>>,
}

struct NativeAbortSignal<'a> {
    cancellation: &'a InferenceCancellation,
    deadline: Option<Instant>,
    #[cfg(test)]
    probe: Option<Arc<NativeAbortTestProbe>>,
}

impl NativeAbortSignal<'_> {
    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

impl<'a> LlamaCppGenerationControl<'a> {
    pub(crate) fn cancellable(cancellation: &'a InferenceCancellation) -> Self {
        Self {
            signal: Some(NativeAbortSignal {
                cancellation,
                deadline: None,
                #[cfg(test)]
                probe: take_native_abort_test_probe(),
            }),
        }
    }

    pub(crate) fn cancellable_until(
        cancellation: &'a InferenceCancellation,
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
        let guard = NativeAbortGuard {
            target_context: Some(target_context),
            draft_context,
            callback_data: Some(signal),
            // Callback teardown must happen on the installing thread.
            _not_send: PhantomData,
        };
        #[cfg(test)]
        if let Some(probe) = &signal.probe {
            probe.record_install_and_wait_for_cancellation(signal.cancellation);
        }
        guard
    }
}

#[cfg(test)]
static NATIVE_ABORT_TEST_PROBE_SLOT: OnceLock<Mutex<Option<Arc<NativeAbortTestProbe>>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) struct NativeAbortTestProbe {
    installed: AtomicUsize,
    callback_calls: AtomicUsize,
    aborting_callback_calls: AtomicUsize,
    install_wait_timed_out: AtomicBool,
    changed: Condvar,
    wait_lock: Mutex<()>,
}

#[cfg(test)]
impl NativeAbortTestProbe {
    pub(crate) fn register() -> Result<(Arc<Self>, NativeAbortTestProbeRegistration), &'static str>
    {
        let probe = Arc::new(Self {
            installed: AtomicUsize::new(0),
            callback_calls: AtomicUsize::new(0),
            aborting_callback_calls: AtomicUsize::new(0),
            install_wait_timed_out: AtomicBool::new(false),
            changed: Condvar::new(),
            wait_lock: Mutex::new(()),
        });
        let mut slot = native_abort_test_probe_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err("a native abort test probe is already registered");
        }
        *slot = Some(Arc::clone(&probe));
        Ok((
            Arc::clone(&probe),
            NativeAbortTestProbeRegistration { probe },
        ))
    }

    pub(crate) fn wait_for_install(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self
            .wait_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while self.installed.load(Ordering::Acquire) == 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, wait) = self
                .changed
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
            if wait.timed_out() && self.installed.load(Ordering::Acquire) == 0 {
                return false;
            }
        }
        true
    }

    pub(crate) fn installed(&self) -> usize {
        self.installed.load(Ordering::Acquire)
    }

    pub(crate) fn callback_calls(&self) -> usize {
        self.callback_calls.load(Ordering::Acquire)
    }

    pub(crate) fn aborting_callback_calls(&self) -> usize {
        self.aborting_callback_calls.load(Ordering::Acquire)
    }

    pub(crate) fn install_wait_timed_out(&self) -> bool {
        self.install_wait_timed_out.load(Ordering::Acquire)
    }

    fn record_install_and_wait_for_cancellation(&self, cancellation: &InferenceCancellation) {
        self.installed.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_all();

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut guard = self
            .wait_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !cancellation.is_cancelled() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.install_wait_timed_out.store(true, Ordering::Release);
                break;
            }
            let (next, _) = self
                .changed
                .wait_timeout(guard, remaining.min(Duration::from_millis(10)))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
        }
    }

    fn record_callback(&self, aborting: bool) {
        self.callback_calls.fetch_add(1, Ordering::AcqRel);
        if aborting {
            self.aborting_callback_calls.fetch_add(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
pub(crate) struct NativeAbortTestProbeRegistration {
    probe: Arc<NativeAbortTestProbe>,
}

#[cfg(test)]
impl Drop for NativeAbortTestProbeRegistration {
    fn drop(&mut self) {
        let mut slot = native_abort_test_probe_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|registered| Arc::ptr_eq(registered, &self.probe))
        {
            *slot = None;
        }
    }
}

#[cfg(test)]
fn native_abort_test_probe_slot() -> &'static Mutex<Option<Arc<NativeAbortTestProbe>>> {
    NATIVE_ABORT_TEST_PROBE_SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn take_native_abort_test_probe() -> Option<Arc<NativeAbortTestProbe>> {
    native_abort_test_probe_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
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
    let aborting = signal.is_cancelled()
        || signal
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
    #[cfg(test)]
    if let Some(probe) = &signal.probe {
        probe.record_callback(aborting);
    }
    aborting
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_control_observes_admitted_job_flag() {
        let cancelled = InferenceCancellation::new();
        let control = LlamaCppGenerationControl::cancellable(&cancelled);

        assert!(control.ensure_running().is_ok());
        cancelled.cancel();

        let error = control.ensure_running().unwrap_err();
        assert!(
            error
                .downcast_ref::<LlamaCppGenerationCancelled>()
                .is_some()
        );
    }

    #[test]
    fn native_abort_callback_reads_the_current_flag_value() {
        let cancelled = InferenceCancellation::new();
        let signal = NativeAbortSignal {
            cancellation: &cancelled,
            deadline: None,
            probe: None,
        };
        let data = std::ptr::from_ref(&signal).cast_mut().cast::<c_void>();

        assert!(!unsafe { llama_abort_callback(data) });
        cancelled.cancel();
        assert!(unsafe { llama_abort_callback(data) });
    }

    #[test]
    fn native_abort_callback_observes_expired_deadline() {
        let cancelled = InferenceCancellation::new();
        let signal = NativeAbortSignal {
            cancellation: &cancelled,
            deadline: Some(Instant::now()),
            probe: None,
        };
        let data = std::ptr::from_ref(&signal).cast_mut().cast::<c_void>();

        assert!(unsafe { llama_abort_callback(data) });
    }
}
