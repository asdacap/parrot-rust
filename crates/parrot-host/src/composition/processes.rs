//! Scoped process observation and the production filename dependency.

use std::sync::Arc;

use parrot_domain::contracts::{
    filenames::FilenameGenerator,
    status::{ActiveWorkObservation, ProcessStatusSnapshot, ProcessStatusSource},
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
}

impl ProcessStatusSource for ScopedProcessStatus {
    fn active(&self) -> Vec<ActiveWorkObservation> {
        self.owner.observe_active()
    }

    fn snapshot(&self) -> Vec<ProcessStatusSnapshot> {
        self.owner.observe_snapshot()
    }
}

/// Supplies the real process filename provider through the domain-only port
/// consumed by storage. Storage remains independent of `parrot-tools`.
pub fn production_filename_generator() -> Arc<dyn FilenameGenerator> {
    Arc::new(HaikunatorNameGenerator::default())
}
