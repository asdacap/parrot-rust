//! Per-agent process names and joined ownership, independent of tool schemas.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_domain::values::identifiers::Identifier;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::execution::{Execution, Failure, ProcessLifetime};
use super::inventory::{
    ActiveShellProcessState, InventorySubscription, ShellProcessInventory,
    ShellProcessInventorySnapshot,
};
use super::managed::{
    ClaimedProcess, CompletionSink, ManagedShellProcess, ProcessDiagnostic, ProcessDiagnostics,
    ProcessReport,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessAgent {
    pub session_id: String,
    pub name: String,
    pub parent_session_id: String,
    pub parent_name: String,
    pub depth: i32,
}

#[derive(Clone, Debug)]
pub struct ProcessStart {
    pub requested_name: Option<String>,
    pub command: String,
    pub description: String,
    pub origin_tool_call_id: String,
    pub agent: ProcessAgent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStatusState {
    Running,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveProcessObservation {
    pub id: String,
    pub name: String,
    pub state: ProcessStatusState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessStatus {
    pub owner_session_id: String,
    pub process_id: String,
    pub name: String,
    pub state: ProcessStatusState,
}

#[derive(Default)]
struct Registry {
    owned: Vec<Arc<ManagedShellProcess>>,
    generated: u64,
    settling: bool,
}

#[derive(Default)]
struct Settlement {
    started: bool,
    tasks: Vec<JoinHandle<io::Result<()>>>,
    failure: Option<Failure>,
    result: Option<Result<(), Failure>>,
}

/// The scope must await `settle`/`dispose`; cancelling a caller's observer does
/// not detach process work or release the scope's resources early.
pub struct ShellProcessOwner {
    session_id: String,
    inventory: Arc<ShellProcessInventory>,
    lifetime: Arc<ProcessLifetime>,
    sink: Arc<dyn CompletionSink>,
    diagnostics: Arc<dyn ProcessDiagnostics>,
    registry: Mutex<Registry>,
    processes: StdMutex<BTreeMap<String, Arc<ManagedShellProcess>>>,
    settlement: Mutex<Settlement>,
}

impl ShellProcessOwner {
    pub fn new(
        session_id: impl Into<String>,
        parent_lifetime: Option<Arc<dyn Cancellation>>,
        sink: Arc<dyn CompletionSink>,
        diagnostics: Arc<dyn ProcessDiagnostics>,
    ) -> io::Result<Self> {
        let session_id = session_id.into();
        let inventory = Arc::new(ShellProcessInventory::new(session_id.clone())?);
        Ok(Self {
            session_id,
            inventory,
            sink,
            diagnostics,
            lifetime: Arc::new(
                parent_lifetime.map_or_else(ProcessLifetime::default, ProcessLifetime::linked),
            ),
            registry: Mutex::new(Registry::default()),
            processes: StdMutex::new(BTreeMap::new()),
            settlement: Mutex::new(Settlement::default()),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.lifetime.is_cancelled()
    }
    pub fn capture_inventory(&self) -> ShellProcessInventorySnapshot {
        self.inventory.capture()
    }
    pub fn subscribe_inventory(&self) -> InventorySubscription {
        self.inventory.subscribe()
    }

    /// The factory receives only this scope's lifetime. It must construct an
    /// already-authorized execution through the platform integration owner.
    /// Name validation belongs to tools: Some("") and slash names are not
    /// silently rejected or normalized by this local registry.
    pub async fn start<F>(
        &self,
        input: ProcessStart,
        report: Arc<dyn ProcessReport>,
        factory: F,
    ) -> io::Result<ClaimedProcess>
    where
        F: FnOnce(Arc<ProcessLifetime>) -> BoxFuture<'static, io::Result<Arc<dyn Execution>>>,
    {
        let mut registry = self.registry.lock().await;
        if registry.settling || self.lifetime.is_cancelled() {
            return Err(io::Error::other("The user session is shutting down."));
        }
        let name = match input.requested_name {
            Some(name) => name,
            None => loop {
                registry.generated = registry
                    .generated
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("Shell process name sequence exhausted."))?;
                let name = format!("shell-{}", registry.generated);
                if !self.processes.lock().unwrap().contains_key(&name) {
                    break name;
                }
            },
        };
        let existing = self.processes.lock().unwrap().get(&name).cloned();
        if existing.is_some_and(|process| !process.retired()) {
            return Err(io::Error::other(format!(
                "Shell process name '{name}' is already reserved."
            )));
        }
        let process_id = format!("shell-process-{}", Identifier::event_id()?);
        let started = Instant::now();
        self.write_start_diagnostic(&process_id, "start", "started", started, None);
        let startup = factory(Arc::clone(&self.lifetime));
        let execution = match tokio::select! {
            biased;
            execution = startup => execution,
            () = self.lifetime.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled")),
        } {
            Ok(execution) => execution,
            Err(error) => {
                self.write_start_diagnostic(
                    &process_id,
                    "completed",
                    if error.kind() == io::ErrorKind::Interrupted {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    started,
                    Some(&error),
                );
                return Err(error);
            }
        };
        if self.lifetime.is_cancelled() {
            execution.request_cancel();
            let _ = execution.settle().await;
            let error = io::Error::new(io::ErrorKind::Interrupted, "operation cancelled");
            self.write_start_diagnostic(
                &process_id,
                "completed",
                "cancelled",
                started,
                Some(&error),
            );
            return Err(error);
        }
        let state = ActiveShellProcessState {
            process_id: process_id.clone(),
            name: name.clone(),
            command: input.command,
            description: input.description,
            origin_tool_call_id: input.origin_tool_call_id,
            owner_agent_session_id: input.agent.session_id,
            owner_agent_name: input.agent.name,
            parent_agent_session_id: input.agent.parent_session_id,
            parent_agent_name: input.agent.parent_name,
            depth: input.agent.depth,
            started: Instant::now(),
            activity_kind: report.activity_kind(),
        };
        let claimed = match ManagedShellProcess::start(
            state,
            Arc::clone(&execution),
            Arc::clone(&self.inventory),
            report,
            Arc::clone(&self.sink),
            Arc::clone(&self.diagnostics),
            Arc::clone(&self.lifetime),
        ) {
            Ok(claimed) => claimed,
            Err(error) => {
                // A failed publication must not strand the execution which the
                // factory already created, even if a caller retains another Arc.
                execution.request_cancel();
                let _ = execution.settle().await;
                self.write_start_diagnostic(
                    &process_id,
                    "completed",
                    "failed",
                    started,
                    Some(&error),
                );
                return Err(error);
            }
        };
        let process = Arc::clone(claimed.process());
        registry.owned.push(Arc::clone(&process));
        self.processes.lock().unwrap().insert(name, process);
        Ok(claimed)
    }

    pub async fn claim(&self, name: &str) -> io::Result<ClaimedProcess> {
        let _registry = self.registry.lock().await;
        let process = self.processes.lock().unwrap().get(name).cloned();
        let process =
            process.ok_or_else(|| io::Error::other(format!("Unknown shell process '{name}'.")))?;
        process.claim()
    }

    pub async fn active(&self) -> Vec<ActiveProcessObservation> {
        let _registry = self.registry.lock().await;
        self.observe_active()
    }

    pub fn observe_active(&self) -> Vec<ActiveProcessObservation> {
        let processes: Vec<_> = self.processes.lock().unwrap().values().cloned().collect();
        let mut active: Vec<_> = processes
            .iter()
            .filter(|process| !process.retired() && process.blocks_turn())
            .map(|process| ActiveProcessObservation {
                id: format!("{}/{}", self.session_id, process.name()),
                name: process.name().into(),
                state: ProcessStatusState::Running,
            })
            .collect();
        active.sort_by(|left, right| ordinal(&left.id, &right.id));
        active
    }

    pub async fn snapshot(&self) -> Vec<ProcessStatus> {
        let _registry = self.registry.lock().await;
        self.observe_snapshot()
    }

    pub fn observe_snapshot(&self) -> Vec<ProcessStatus> {
        let processes: Vec<_> = self.processes.lock().unwrap().values().cloned().collect();
        let mut snapshot: Vec<_> = processes
            .iter()
            .filter(|process| !process.retired())
            .map(|process| ProcessStatus {
                owner_session_id: self.session_id.clone(),
                process_id: process.state.process_id.clone(),
                name: process.name().into(),
                state: ProcessStatusState::Running,
            })
            .collect();
        snapshot.sort_by(|left, right| ordinal(&left.process_id, &right.process_id));
        snapshot
    }

    pub async fn settle(&self) -> io::Result<()> {
        // Startup may be awaiting this very lifetime while holding the registry.
        // Cancellation must therefore precede every startup/settlement gate.
        self.lifetime.cancel();
        let mut settlement = self.settlement.lock().await;
        if let Some(result) = &settlement.result {
            return result.as_ref().map(|_| ()).map_err(Failure::error);
        }
        if !settlement.started {
            let processes = {
                let mut registry = self.registry.lock().await;
                registry.settling = true;
                self.lifetime.cancel();
                registry.owned.clone()
            };
            // Start every cleanup before awaiting any one; an earlier stall or
            // error cannot prevent cancellation of later or retired instances.
            for process in processes {
                process.request_cancel();
                settlement
                    .tasks
                    .push(tokio::spawn(async move { process.settle().await }));
            }
            settlement.started = true;
        }
        while let Some(task) = settlement.tasks.last_mut() {
            let result = task
                .await
                .map_err(io::Error::other)
                .and_then(|result| result);
            if let Err(error) = result {
                settlement
                    .failure
                    .get_or_insert_with(|| Failure::capture(&error));
            }
            settlement.tasks.pop();
        }
        let result = settlement
            .failure
            .as_ref()
            .map_or(Ok(()), |error| Err(error.error()));
        settlement.result = Some(result.as_ref().map(|_| ()).map_err(Failure::capture));
        result
    }

    /// Publication termination is deliberately independent of settlement.
    pub async fn dispose(&self) -> io::Result<()> {
        self.inventory.dispose();
        self.lifetime.cancel();
        self.settle().await
    }

    fn write_start_diagnostic(
        &self,
        process_id: &str,
        operation: &'static str,
        outcome: &'static str,
        started: Instant,
        error: Option<&io::Error>,
    ) {
        self.diagnostics.write(ProcessDiagnostic {
            operation,
            outcome,
            owner_agent_session_id: self.session_id.clone(),
            process_id: process_id.into(),
            elapsed_milliseconds: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            error_kind: error.map(io::Error::kind),
        });
    }
}

impl Drop for ShellProcessOwner {
    fn drop(&mut self) {
        self.inventory.dispose();
        self.lifetime.cancel();
        if let Ok(registry) = self.registry.try_lock() {
            for process in &registry.owned {
                process.request_cancel();
            }
        }
    }
}

/// Explicit scope resolution is delegated to T48; an explicit path failure
/// never falls back to this owner. Bare names remain local to this owner.
pub async fn claim_address<F>(
    local: &Arc<ShellProcessOwner>,
    address: &str,
    resolve: F,
) -> io::Result<ClaimedProcess>
where
    F: FnOnce(&str) -> BoxFuture<'static, io::Result<(Arc<ShellProcessOwner>, String)>>,
{
    if address.contains('/') {
        let (owner, name) = resolve(address).await?;
        owner.claim(&name).await
    } else {
        local.claim(address).await
    }
}

fn ordinal(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

#[cfg(test)]
#[path = "../../../tests/process/ownership/managed.rs"]
mod tests;
