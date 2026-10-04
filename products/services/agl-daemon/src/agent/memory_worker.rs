//! Bounded terminal publication outside the Run scheduler's active slots.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use agl_core::AgentRunId;

const PUBLICATION_QUEUE_CAPACITY: usize = 256;

#[derive(Clone)]
pub(crate) struct PublicationQueue {
    sender: mpsc::SyncSender<AgentRunId>,
    stopped: Arc<AtomicBool>,
}

impl PublicationQueue {
    pub(super) fn submit(&self, run_id: AgentRunId) -> Result<(), &'static str> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("memory publication deferred: daemon is shutting down");
        }
        self.sender.try_send(run_id).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => "memory publication deferred: queue is full",
            mpsc::TrySendError::Disconnected(_) => "memory publication deferred: worker stopped",
        })
    }
}

pub(super) struct PublicationWorker {
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PublicationWorker {
    pub(super) fn start(
        publish: impl Fn(AgentRunId) + Send + 'static,
    ) -> std::io::Result<(Self, PublicationQueue)> {
        let stopped = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(PUBLICATION_QUEUE_CAPACITY);
        let worker_stop = stopped.clone();
        let worker = thread::Builder::new()
            .name("agl-memory-publication".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(run_id) => {
                            if worker_stop.load(Ordering::Acquire) {
                                break;
                            }
                            publish(run_id);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        Ok((
            Self {
                stopped: stopped.clone(),
                worker: Some(worker),
            },
            PublicationQueue { sender, stopped },
        ))
    }

    pub(super) fn stop_accepting(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

impl Drop for PublicationWorker {
    fn drop(&mut self) {
        self.stop_accepting();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_publication_has_bounded_nonblocking_admission_and_shutdown_skips_queue() {
        let (entered, observed) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (worker, queue) = PublicationWorker::start(move |run_id| {
            entered.send(run_id).unwrap();
            blocked.recv().unwrap();
        })
        .unwrap();
        let first = AgentRunId::generate();
        queue.submit(first).unwrap();
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            first
        );
        for _ in 0..PUBLICATION_QUEUE_CAPACITY {
            queue.submit(AgentRunId::generate()).unwrap();
        }
        assert!(
            queue
                .submit(AgentRunId::generate())
                .unwrap_err()
                .contains("full")
        );
        worker.stop_accepting();
        assert!(
            queue
                .submit(AgentRunId::generate())
                .unwrap_err()
                .contains("shutting down")
        );
        release.send(()).unwrap();
        drop(worker);
        assert!(observed.try_recv().is_err());
    }
}
