//! The managed-owner boundary over the real native execution component.

use std::future::pending;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use tokio::sync::Notify;

pub use crate::process::execution::ProcessResult;
use crate::process::execution::ShellProcessExecution;

/// Observation cancellation never changes ownership of an execution.
pub trait Execution: Send + Sync {
    fn is_completed(&self) -> bool;
    fn is_pseudo_terminal(&self) -> bool;
    fn stdout_path(&self) -> Option<PathBuf>;
    fn stderr_path(&self) -> Option<PathBuf>;
    fn read_transcript(&self, offset: u64) -> io::Result<(u64, Vec<u16>)>;
    fn read_result(&self, offset: u64) -> io::Result<(u64, ProcessResult)>;
    fn wait(&self) -> BoxFuture<'_, io::Result<ProcessResult>>;
    fn write_stdin<'a>(&'a self, input: &'a str) -> BoxFuture<'a, io::Result<()>>;
    fn send_signal(&self, signal: i32) -> io::Result<()>;
    fn request_cancel(&self);
    fn settle(&self) -> BoxFuture<'_, io::Result<()>>;
}

impl Execution for ShellProcessExecution {
    fn is_completed(&self) -> bool {
        self.is_completed()
    }
    fn is_pseudo_terminal(&self) -> bool {
        self.is_pseudo_terminal()
    }
    fn stdout_path(&self) -> Option<PathBuf> {
        self.stdout_path()
    }
    fn stderr_path(&self) -> Option<PathBuf> {
        self.stderr_path()
    }
    fn read_transcript(&self, offset: u64) -> io::Result<(u64, Vec<u16>)> {
        self.read_transcript(offset)
    }
    fn read_result(&self, offset: u64) -> io::Result<(u64, ProcessResult)> {
        self.read_result(offset)
    }
    fn wait(&self) -> BoxFuture<'_, io::Result<ProcessResult>> {
        Box::pin(self.wait())
    }
    fn write_stdin<'a>(&'a self, input: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(self.write_stdin(input))
    }
    fn send_signal(&self, signal: i32) -> io::Result<()> {
        self.send_signal(signal)
    }
    fn request_cancel(&self) {
        self.request_cancel();
    }
    fn settle(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(self.settle())
    }
}

/// Cancelling the owner does not cancel its parent scope.
#[derive(Default)]
pub struct ProcessLifetime {
    cancelled: AtomicBool,
    changed: Notify,
    parent: Option<Arc<dyn Cancellation>>,
}

impl ProcessLifetime {
    pub fn linked(parent: Arc<dyn Cancellation>) -> Self {
        Self {
            parent: Some(parent),
            ..Self::default()
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn local_cancelled(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.cancelled.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

impl Cancellation for ProcessLifetime {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.is_cancelled())
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Some(parent) = &self.parent {
                tokio::select! {
                    () = self.local_cancelled() => {},
                    () = parent.cancelled() => {},
                }
            } else {
                self.local_cancelled().await;
            }
        })
    }
}

pub struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Failure {
    pub kind: io::ErrorKind,
    pub message: String,
}
impl Failure {
    pub fn capture(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
    pub fn error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}
