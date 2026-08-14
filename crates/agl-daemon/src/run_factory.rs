use agl_chat::ChatSupervisorFactory;
use agl_store::DurableRunRecord;
use agl_supervisor::{DurableRunDriver, DurableRunDriverFactory, Result, RunCancellation};

#[derive(Clone)]
pub(crate) struct DaemonRunFactory {
    chat: ChatSupervisorFactory,
}

impl DaemonRunFactory {
    pub(crate) fn new(chat: ChatSupervisorFactory) -> Self {
        Self { chat }
    }
}

impl DurableRunDriverFactory for DaemonRunFactory {
    fn open(
        &self,
        run: &DurableRunRecord,
        cancellation: RunCancellation,
    ) -> Result<Box<dyn DurableRunDriver>> {
        self.chat.open(run, cancellation)
    }
}
