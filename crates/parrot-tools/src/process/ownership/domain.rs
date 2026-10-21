//! Runtime-facing domain contracts over the actual scoped process owner.
//!
//! This adapter neither captures nor grants filesystem authority. The host
//! supplies a launcher which consumes the invocation's existing security value.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::contracts::processes::{ManagedProcess, ProcessOwner};
use parrot_domain::contracts::security::AccessSnapshot;

use super::execution::{Execution, NeverCancelled, ProcessLifetime};
use super::managed::{
    ClaimedProcess, ManagedProcessSnapshot, ManagedShellProcess, ProcessReport, ShellWaitResult,
};
use super::owner::{ProcessStart, ShellProcessOwner};

/// The platform/host boundary. Implementations must own partial startup until it
/// is published or cleaned up, including when this future is dropped. The real
/// native PTY startup future already has this ownership contract.
///
/// `security` is the original owned invocation capture, not a replacement policy
/// or a mutable provider. Selecting and enforcing the sandbox remain the
/// launcher's responsibility; this adapter does not authorize a launch.
pub trait CapturedProcessLauncher: Send + Sync + 'static {
    type Request: Send + 'static;
    type Security: AccessSnapshot + 'static;

    fn launch(
        &self,
        request: Self::Request,
        security: Self::Security,
        lifetime: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>>;
}

/// Owner metadata and the actual platform-specific launch request travel
/// together; no command/environment/terminal option is reconstructed here.
pub struct ProcessStartRequest<Request> {
    pub process: ProcessStart,
    pub report: Arc<dyn ProcessReport>,
    pub launch: Request,
}

pub struct DomainProcessOwner<Launcher: CapturedProcessLauncher> {
    owner: Arc<ShellProcessOwner>,
    launcher: Arc<Launcher>,
}

impl<Launcher: CapturedProcessLauncher> DomainProcessOwner<Launcher> {
    pub fn new(owner: Arc<ShellProcessOwner>, launcher: Arc<Launcher>) -> Self {
        Self { owner, launcher }
    }

    pub fn owner(&self) -> &Arc<ShellProcessOwner> {
        &self.owner
    }

    pub async fn claim(&self, name: &str) -> io::Result<DomainManagedProcess> {
        let claim = self.owner.claim(name).await?;
        Ok(DomainManagedProcess::new(self.owner.session_id(), claim))
    }
}

impl<Launcher: CapturedProcessLauncher> ProcessOwner for DomainProcessOwner<Launcher> {
    type StartRequest = ProcessStartRequest<Launcher::Request>;
    type Security = Launcher::Security;
    type Process = DomainManagedProcess;
    type Error = io::Error;

    fn start<'a>(
        &'a self,
        request: Self::StartRequest,
        security: Self::Security,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::Process, OperationError<Self::Error>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let ProcessStartRequest {
                process,
                report,
                launch,
            } = request;
            let launcher = Arc::clone(&self.launcher);
            let startup = self.owner.start(process, report, move |lifetime| {
                Box::pin(async move { launcher.launch(launch, security, lifetime).await })
            });
            // A ready start wins the race, so its initial claim is transferred
            // into the returned handle rather than losing a published process.
            let claim = tokio::select! {
                biased;
                result = startup => result.map_err(|error| operation_error(
                    error,
                    cancellation.is_cancelled() || self.owner.is_cancelled(),
                ))?,
                () = cancellation.cancelled() => return Err(OperationError::Cancelled),
            };
            Ok(DomainManagedProcess::new(self.owner.session_id(), claim))
        })
    }

    fn settle(&self) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(self.owner.settle())
    }
}

/// A handle is bound to a physical managed instance, not a reusable name lookup.
/// The initial start claim is retained until an observation or handle drop.
/// Cancelling/dropping an observation releases its RAII claim, not the owner's
/// execution. Later observations acquire a fresh claim on the same instance.
pub struct DomainManagedProcess {
    owner: String,
    process: Arc<ManagedShellProcess>,
    initial_claim: Mutex<Option<ClaimedProcess>>,
}

impl DomainManagedProcess {
    fn new(owner: &str, claim: ClaimedProcess) -> Self {
        Self {
            owner: owner.into(),
            process: Arc::clone(claim.process()),
            initial_claim: Mutex::new(Some(claim)),
        }
    }

    fn claim(&self) -> io::Result<ClaimedProcess> {
        let initial = self
            .initial_claim
            .lock()
            .map_err(|_| io::Error::other("The process claim lock is poisoned."))?
            .take();
        initial.map_or_else(|| self.process.claim(), Ok)
    }
}

impl ManagedProcess for DomainManagedProcess {
    type Owner = String;
    type Signal = i32;
    type WaitResult = ShellWaitResult;
    type Snapshot = ManagedProcessSnapshot;
    type Error = io::Error;

    fn owner(&self) -> &Self::Owner {
        &self.owner
    }

    fn snapshot(&self) -> Result<Self::Snapshot, Self::Error> {
        self.process.snapshot()
    }

    fn write_stdin<'a>(
        &'a self,
        input: String,
        yield_after: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::WaitResult, OperationError<Self::Error>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.claim()
                .map_err(OperationError::Failed)?
                .write_stdin(&input, yield_after, cancellation)
                .await
                .map_err(|error| {
                    operation_error(
                        error,
                        cancellation.is_cancelled() || self.process.is_cancelled(),
                    )
                })
        })
    }

    fn signal(&self, signal: Self::Signal) -> Result<(), Self::Error> {
        self.claim()?.send_signal(signal, &NeverCancelled)
    }

    fn wait<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Self::WaitResult, OperationError<Self::Error>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.claim()
                .map_err(OperationError::Failed)?
                .wait(None, cancellation)
                .await
                .map_err(|error| {
                    operation_error(
                        error,
                        cancellation.is_cancelled() || self.process.is_cancelled(),
                    )
                })
        })
    }

    fn settle(&self) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            // Release an unobserved start claim before joining delivery; the
            // delivery task must not wait forever for this very handle.
            let claim = self
                .initial_claim
                .lock()
                .map_err(|_| io::Error::other("The process claim lock is poisoned."))?
                .take();
            drop(claim);
            // Joining one process is not an implicit signal. Scope shutdown
            // cancels the owner before joining all of its processes.
            self.process.settle().await
        })
    }
}

fn operation_error(error: io::Error, cancelled: bool) -> OperationError<io::Error> {
    if cancelled && error.kind() == io::ErrorKind::Interrupted {
        OperationError::Cancelled
    } else {
        OperationError::Failed(error)
    }
}
