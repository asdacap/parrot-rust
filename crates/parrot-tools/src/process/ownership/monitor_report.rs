//! The source monitor report joined to a managed owner's real execution.
//!
//! Construct one report per process, as the source monitor factory does. Agent
//! delivery, configured templates and blob persistence remain injected services.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::clock::SystemClock;

use super::execution::Execution;
use super::inventory::{ActiveShellProcessState, ActivityKind};
use super::managed::ProcessReport;
use super::monitor::{MonitorIdentity, MonitorReport, MonitorSink, ShellProcessMonitor};

pub struct MonitorProcessReport {
    timeout: Duration,
    sink: Arc<dyn MonitorSink>,
    report: Mutex<MonitorReport>,
}

impl MonitorProcessReport {
    pub fn new(timeout: Duration, sink: Arc<dyn MonitorSink>) -> Self {
        Self {
            timeout,
            sink,
            report: Mutex::new(
                ShellProcessMonitor::new(timeout, Arc::new(SystemClock))
                    .report()
                    .clone(),
            ),
        }
    }
}

fn identity(state: &ActiveShellProcessState) -> MonitorIdentity {
    MonitorIdentity {
        name: state.name.clone(),
        description: state.description.clone(),
        owner_agent_session_id: state.owner_agent_session_id.clone(),
        process_id: state.process_id.clone(),
    }
}

impl ProcessReport for MonitorProcessReport {
    fn blocks_turn(&self) -> bool {
        true
    }

    fn activity_kind(&self) -> ActivityKind {
        ActivityKind::Monitor
    }

    fn observe<'a>(
        &'a self,
        state: &'a ActiveShellProcessState,
        execution: Arc<dyn Execution>,
        lifetime: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let mut monitor = ShellProcessMonitor::new(self.timeout, Arc::new(SystemClock));
            monitor
                .observe(
                    &identity(state),
                    execution.as_ref(),
                    self.sink.as_ref(),
                    lifetime,
                )
                .await;
            // No synchronous lock is held while observing or invoking services.
            *self
                .report
                .lock()
                .map_err(|_| io::Error::other("The monitor report lock is poisoned."))? =
                monitor.report().clone();
            Ok(())
        })
    }

    fn complete(&self, state: &ActiveShellProcessState, _output: &str) -> io::Result<String> {
        let report = self
            .report
            .lock()
            .map_err(|_| io::Error::other("The monitor report lock is poisoned."))?
            .clone();
        self.sink.render_completion(&identity(state), &report)
    }
}

#[cfg(test)]
#[path = "../../../tests/process/ownership/monitor_report.rs"]
mod tests;
