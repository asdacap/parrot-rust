//! Completed actor adapter for T23's one cached synchronous coordinator.
use super::*;
use crate::diagnostics::statistics::UserSessionStatistics;
use parrot_domain::values::accounting::{AgentSessionStatistics, AgentUsageIncrement};
use std::collections::HashMap;

type StatisticsError = Box<dyn std::error::Error + Send + Sync>;
impl EventRepository {
    /// Shared by repository clones. The coordinator's synchronous append callback
    /// is used by record_usage below on a blocking-pool thread, never the executor.
    pub async fn get_runtime_statistics(&self) -> Result<Arc<UserSessionStatistics>, EventError> {
        self.get_runtime_statistics_with_progress(None).await
    }

    /// Observes event loading only when initializing the shared coordinator.
    pub async fn get_runtime_statistics_with_progress(
        &self,
        progress: Option<EventLoadProgress>,
    ) -> Result<Arc<UserSessionStatistics>, EventError> {
        self.load_runtime_statistics(HashMap::new(), progress).await
    }

    /// Restores the shared coordinator with canonical root ancestry before any root facts exist.
    pub async fn restore_runtime_statistics(
        &self,
        root_agent_session_id: String,
        progress: Option<EventLoadProgress>,
    ) -> Result<Arc<UserSessionStatistics>, EventError> {
        self.load_runtime_statistics(
            HashMap::from([(root_agent_session_id, String::new())]),
            progress,
        )
        .await
    }

    async fn load_runtime_statistics(
        &self,
        parents: HashMap<String, String>,
        progress: Option<EventLoadProgress>,
    ) -> Result<Arc<UserSessionStatistics>, EventError> {
        let statistics = self
            .inner
            .runtime_statistics
            .get_or_try_init(|| async {
                let replay = self
                    .transaction(move |tx| {
                        statistics::replay_statistics_with_parents(tx, parents, progress)
                    })
                    .await?;
                let database = self.database().clone();
                let runtime = tokio::runtime::Handle::try_current()
                    .map_err(|error| EventError::Invalid(error.to_string()))?;
                Ok::<_, EventError>(Arc::new(UserSessionStatistics::new(replay, move |event| {
                    let event = event.clone();
                    let ticket = database
                        .admit_with_error((), &NeverCancelled, move |tx| {
                            statistics::append_usage_fact(tx, &event, &timestamp)
                        })
                        .map_err(|error| -> StatisticsError {
                            Box::new(match error {
                                AdmissionError::Rejected(error) => error,
                                AdmissionError::Cancelled => {
                                    unreachable!("uncancelled completed append")
                                }
                            })
                        })?;
                    // The outer facade runs the synchronous coordinator on the
                    // blocking pool. Waiting here observes an ACTUAL commit receipt.
                    runtime.block_on(async move {
                        Ok(ticket
                            .into_completion()
                            .await
                            .map_err(|error| -> StatisticsError { Box::new(error) })?
                            .into_value()
                            .into_output())
                    })
                })))
            })
            .await?;
        Ok(Arc::clone(statistics))
    }

    /// The synchronous T23 append→apply→publish gate runs as one owned blocking
    /// operation. Dropping this await does not cancel an already admitted append.
    /// Supply actual live handles and ancestor apply callback at composition.
    pub async fn record_usage<P, A>(
        &self,
        fact: Event,
        increment: AgentUsageIncrement,
        agent: Arc<AgentSessionStatistics>,
        root: Arc<AgentSessionStatistics>,
        apply: A,
        publish: P,
    ) -> Result<bool, StatisticsError>
    where
        P: Fn(Event) + Send + 'static,
        A: FnOnce(&AgentUsageIncrement) + Send + 'static,
    {
        let coordinator = self
            .get_runtime_statistics()
            .await
            .map_err(|error| -> StatisticsError { Box::new(error) })?;
        tokio::task::spawn_blocking(move || {
            coordinator.record_usage(&publish, fact, &increment, apply, &agent, &root)
        })
        .await
        .map_err(|error| -> StatisticsError { Box::new(error) })?
    }
}
