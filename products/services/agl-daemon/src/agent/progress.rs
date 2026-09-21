use super::*;

pub(crate) type ProgressSubscribers = Arc<Mutex<Vec<ProgressSubscriber>>>;

pub(crate) struct ProgressSubscriber {
    pub(crate) run_id: Option<AgentRunId>,
    pub(crate) sender: tokio::sync::mpsc::Sender<SubscriptionSignal>,
    pub(crate) lagged: Arc<AtomicBool>,
}

pub struct AgentProgressSubscription {
    pub(crate) receiver: tokio::sync::mpsc::Receiver<SubscriptionSignal>,
    pub(crate) lagged: Arc<AtomicBool>,
}

impl AgentProgressSubscription {
    pub fn try_recv(
        &mut self,
    ) -> Result<SubscriptionSignal, tokio::sync::mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    pub async fn recv(&mut self) -> Option<SubscriptionSignal> {
        self.receiver.recv().await
    }

    pub fn take_lagged(&self) -> bool {
        self.lagged.swap(false, Ordering::AcqRel)
    }
}

pub enum SubscriptionSignal {
    Progress(AgentProgress),
    Durable,
}

pub(crate) fn emit_progress(subscribers: &ProgressSubscribers, progress: AgentProgress) {
    let run_id = match &progress {
        AgentProgress::ModelOutputDelta { operation, .. }
        | AgentProgress::OperationStatus { operation, .. } => operation.run_id,
    };
    if let Ok(mut subscribers) = subscribers.lock() {
        subscribers.retain(|subscriber| {
            if subscriber.run_id.is_some_and(|selected| selected != run_id) {
                return true;
            }
            match subscriber
                .sender
                .try_send(SubscriptionSignal::Progress(progress.clone()))
            {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    subscriber.lagged.store(true, Ordering::Release);
                    true
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }
}

pub(crate) fn emit_durable(subscribers: &ProgressSubscribers, run_id: AgentRunId) {
    if let Ok(mut subscribers) = subscribers.lock() {
        subscribers.retain(|subscriber| {
            if subscriber.run_id.is_some_and(|selected| selected != run_id) {
                return true;
            }
            match subscriber.sender.try_send(SubscriptionSignal::Durable) {
                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }
}
