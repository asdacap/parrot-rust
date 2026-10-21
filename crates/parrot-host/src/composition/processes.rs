//! Scoped process observation and the production filename dependency.

use std::sync::Arc;

use parrot_domain::contracts::{
    filenames::FilenameGenerator,
    status::{ActiveWorkObservation, ActiveWorkState, ProcessStatusSnapshot, ProcessStatusSource},
};
use parrot_tools::process::{
    execution::filenames::HaikunatorNameGenerator, ownership::owner::ShellProcessOwner,
};

/// Observation stays bound to the same actual scoped process owner used to
/// start, claim and settle work. It does not inspect the operating system.
pub struct ScopedProcessStatus {
    owner: Arc<ShellProcessOwner>,
}

impl ScopedProcessStatus {
    pub fn new(owner: Arc<ShellProcessOwner>) -> Self {
        Self { owner }
    }

    pub fn owner(&self) -> &Arc<ShellProcessOwner> {
        &self.owner
    }
}

impl ProcessStatusSource for ScopedProcessStatus {
    fn active(&self) -> Vec<ActiveWorkObservation> {
        self.owner
            .observe_active()
            .into_iter()
            .map(|process| ActiveWorkObservation {
                id: process.id,
                name: process.name,
                state: ActiveWorkState::Running,
            })
            .collect()
    }

    fn snapshot(&self) -> Vec<ProcessStatusSnapshot> {
        self.owner
            .observe_snapshot()
            .into_iter()
            .map(|process| ProcessStatusSnapshot {
                owner_session_id: process.owner_session_id,
                process_id: process.process_id,
                name: process.name,
                state: ActiveWorkState::Running,
            })
            .collect()
    }
}

/// Supplies the real process filename provider through the domain-only port
/// consumed by storage. Storage remains independent of `parrot-tools`.
pub fn production_filename_generator() -> Arc<dyn FilenameGenerator> {
    Arc::new(HaikunatorNameGenerator::default())
}
