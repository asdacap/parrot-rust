//! Deferred regressions for the real frozen-domain adapter. These captures deny
//! every access check: the tests do not supply or claim filesystem authority.

use super::{FakeExecution, Sink, input, owner};
use crate::process::ownership::domain::{
    CapturedProcessLauncher, DomainProcessOwner, ProcessStartRequest,
};
use crate::process::ownership::execution::{Execution, NeverCancelled, ProcessLifetime};
use crate::process::ownership::managed::CompletionReport;
use parrot_domain::contracts::async_ops::{BoxFuture, OperationError};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::processes::{ManagedProcess, ProcessOwner};
use parrot_domain::contracts::security::AccessSnapshot;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct CapturedSecurity(Box<u64>);
impl AccessSnapshot for CapturedSecurity {
    type Error = io::Error;

    fn check_read(&self, _: &ResolvedRead) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test capture denies access",
        ))
    }

    fn check_write(&self, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test capture denies access",
        ))
    }
}

struct Launcher {
    execution: Arc<FakeExecution>,
    received: Mutex<Option<(usize, u64, String)>>,
}
impl CapturedProcessLauncher for Launcher {
    type Request = String;
    type Security = CapturedSecurity;

    fn launch(
        &self,
        request: String,
        security: CapturedSecurity,
        _lifetime: Arc<ProcessLifetime>,
    ) -> BoxFuture<'_, io::Result<Arc<dyn Execution>>> {
        Box::pin(async move {
            *self.received.lock().unwrap() = Some((
                security.0.as_ref() as *const u64 as usize,
                *security.0,
                request,
            ));
            Ok(Arc::clone(&self.execution) as Arc<dyn Execution>)
        })
    }
}

#[tokio::test]
async fn domain_start_moves_the_actual_capture_and_keeps_the_initial_physical_claim() {
    let owner = owner("domain-owner", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    let launcher = Arc::new(Launcher {
        execution: Arc::clone(&execution),
        received: Mutex::new(None),
    });
    let adapter = DomainProcessOwner::new(Arc::clone(&owner), Arc::clone(&launcher));
    let security = CapturedSecurity(Box::new(73));
    let address = security.0.as_ref() as *const u64 as usize;
    let process = ProcessOwner::start(
        &adapter,
        ProcessStartRequest {
            process: input("domain-owner", Some("build")),
            report: Arc::new(CompletionReport),
            launch: "actual platform request".into(),
        },
        security,
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert_eq!(
        *launcher.received.lock().unwrap(),
        Some((address, 73, "actual platform request".into()))
    );
    assert_eq!(ManagedProcess::owner(&process), "domain-owner");
    assert!(owner.claim("build").await.is_err());
    let snapshot = ManagedProcess::snapshot(&process).unwrap();
    assert_eq!(snapshot.process.name, "build");
    assert!(!snapshot.completed);
    assert!(!snapshot.retired);
    ManagedProcess::signal(&process, 15).unwrap();
    assert_eq!(*execution.signals.lock().unwrap(), vec![15]);
    let next_claim = adapter.claim("build").await.unwrap();
    assert_eq!(
        ManagedProcess::snapshot(&next_claim)
            .unwrap()
            .process
            .process_id,
        snapshot.process.process_id
    );
    drop(next_claim);
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn cancelled_domain_observation_does_not_consume_the_start_claim_or_cancel_execution() {
    let owner = owner("domain-cancel", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    let launcher = Arc::new(Launcher {
        execution: Arc::clone(&execution),
        received: Mutex::new(None),
    });
    let adapter = DomainProcessOwner::new(Arc::clone(&owner), launcher);
    let process = ProcessOwner::start(
        &adapter,
        ProcessStartRequest {
            process: input("domain-cancel", Some("build")),
            report: Arc::new(CompletionReport),
            launch: String::new(),
        },
        CapturedSecurity(Box::new(1)),
        &NeverCancelled,
    )
    .await
    .unwrap();
    let cancellation = ProcessLifetime::default();
    cancellation.cancel();
    assert!(matches!(
        ManagedProcess::wait(&process, &cancellation).await,
        Err(OperationError::Cancelled)
    ));
    assert_eq!(execution.cancel_count.load(Ordering::Acquire), 0);
    assert!(owner.claim("build").await.is_err());
    ManagedProcess::signal(&process, 2).unwrap();
    assert_eq!(*execution.signals.lock().unwrap(), vec![2]);
    owner.dispose().await.unwrap();
}

#[tokio::test]
async fn domain_settlement_releases_an_unobserved_claim_and_joins_real_owned_work() {
    let owner = owner("domain-settle", Arc::new(Sink::default()));
    let execution = Arc::new(FakeExecution::default());
    let launcher = Arc::new(Launcher {
        execution: Arc::clone(&execution),
        received: Mutex::new(None),
    });
    let adapter = DomainProcessOwner::new(Arc::clone(&owner), launcher);
    let process = ProcessOwner::start(
        &adapter,
        ProcessStartRequest {
            process: input("domain-settle", Some("build")),
            report: Arc::new(CompletionReport),
            launch: String::new(),
        },
        CapturedSecurity(Box::new(1)),
        &NeverCancelled,
    )
    .await
    .unwrap();
    execution.complete(7);
    tokio::time::timeout(Duration::from_secs(5), ManagedProcess::settle(&process))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.cancel_count.load(Ordering::Acquire), 0);
    assert_eq!(execution.settle_count.load(Ordering::Acquire), 1);
    assert!(ManagedProcess::snapshot(&process).unwrap().completed);
    ProcessOwner::settle(&adapter).await.unwrap();
    owner.dispose().await.unwrap();
}
