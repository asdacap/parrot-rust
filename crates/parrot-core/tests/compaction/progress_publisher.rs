use parrot_core::{
    BoxFuture, CancellationToken, Error, EventPublisher, compaction::CompactionProgress,
};
use std::sync::{Arc, Mutex};

pub struct ProgressPublisher {
    pub progress: Arc<Mutex<Vec<CompactionProgress>>>,
}

impl EventPublisher for ProgressPublisher {
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Err(Error::unsupported("ProgressPublisher.on_epoch_started")) })
    }
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            Err(Error::unsupported(
                "ProgressPublisher.report_compaction_started",
            ))
        })
    }
    fn report_compaction_failed(&self, _: String) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            Err(Error::unsupported(
                "ProgressPublisher.report_compaction_failed",
            ))
        })
    }
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        progress: CompactionProgress,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.progress.lock().unwrap().push(progress);
            Ok(())
        })
    }
}
