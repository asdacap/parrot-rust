//! Source-shaped owner inventories and capacity-one latest-snapshot subscriptions.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use parrot_domain::values::identifiers::Identifier;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityKind {
    Execution,
    Monitor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveShellProcessState {
    pub process_id: String,
    pub name: String,
    pub command: String,
    pub description: String,
    pub origin_tool_call_id: String,
    pub owner_agent_session_id: String,
    pub owner_agent_name: String,
    pub parent_agent_session_id: String,
    pub parent_agent_name: String,
    pub depth: i32,
    pub started: Instant,
    pub activity_kind: ActivityKind,
}

impl ActiveShellProcessState {
    pub fn elapsed_milliseconds(&self) -> i64 {
        self.started
            .elapsed()
            .as_millis()
            .try_into()
            .expect("process elapsed milliseconds exceed i64")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedShellProcessState {
    pub process_id: String,
    pub elapsed_milliseconds: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellProcessInventorySnapshot {
    pub owner_agent_session_id: String,
    pub inventory_instance_id: String,
    pub revision: u64,
    pub removed: bool,
    pub processes: Vec<ActiveShellProcessState>,
    pub completed_processes: Vec<CompletedShellProcessState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YieldedShellProcess {
    pub process_id: String,
    pub name: String,
    pub inventory_instance_id: String,
    pub visible_revision: u64,
    pub stdout_path: Option<PathBuf>,
    pub stderr_path: Option<PathBuf>,
    pub activity_kind: ActivityKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InventoryError {
    Disposed,
    OwnerMismatch { actual: String, expected: String },
    AlreadyVisible(String),
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disposed => f.write_str("The shell process inventory is disposed."),
            Self::OwnerMismatch { actual, expected } => write!(
                f,
                "Shell process owner '{actual}' does not match inventory owner '{expected}'."
            ),
            Self::AlreadyVisible(id) => write!(f, "Shell process '{id}' is already visible."),
        }
    }
}

impl std::error::Error for InventoryError {}

pub struct ShellProcessInventory {
    inner: Arc<Mutex<InventoryState>>,
}

struct InventoryState {
    owner: String,
    instance: String,
    revision: u64,
    disposed: bool,
    processes: HashMap<String, ActiveShellProcessState>,
    completed: HashMap<String, CompletedShellProcessState>,
    subscribers: HashMap<u64, Weak<Subscriber>>,
    next_subscriber: u64,
}

impl InventoryState {
    fn capture(&self) -> ShellProcessInventorySnapshot {
        let mut processes: Vec<_> = self.processes.values().cloned().collect();
        let mut completed_processes: Vec<_> = self.completed.values().cloned().collect();
        processes.sort_by(|a, b| a.process_id.encode_utf16().cmp(b.process_id.encode_utf16()));
        completed_processes
            .sort_by(|a, b| a.process_id.encode_utf16().cmp(b.process_id.encode_utf16()));
        ShellProcessInventorySnapshot {
            owner_agent_session_id: self.owner.clone(),
            inventory_instance_id: self.instance.clone(),
            revision: self.revision,
            removed: self.disposed,
            processes,
            completed_processes,
        }
    }

    fn publish(&mut self, close: bool) {
        let snapshot = self.capture();
        self.subscribers.retain(|_, subscriber| {
            let Some(subscriber) = subscriber.upgrade() else {
                return false;
            };
            let mut slot = subscriber.slot.lock().expect("inventory subscription lock");
            if slot.closed {
                return false;
            }
            slot.pending = Some(snapshot.clone());
            slot.closed = close;
            subscriber.changed.notify_one();
            !close
        });
    }
}

impl ShellProcessInventory {
    pub fn new(owner_agent_session_id: impl Into<String>) -> io::Result<Self> {
        Ok(Self::with_instance_id(
            owner_agent_session_id,
            format!("process-inventory-{}", Identifier::event_id()?),
        ))
    }

    pub fn with_instance_id(
        owner_agent_session_id: impl Into<String>,
        instance_id: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InventoryState {
                owner: owner_agent_session_id.into(),
                instance: instance_id.into(),
                revision: 0,
                disposed: false,
                processes: HashMap::new(),
                completed: HashMap::new(),
                subscribers: HashMap::new(),
                next_subscriber: 0,
            })),
        }
    }

    pub fn publish(
        &self,
        process: ActiveShellProcessState,
    ) -> Result<YieldedShellProcess, InventoryError> {
        let mut state = self.inner.lock().expect("process inventory lock");
        if state.disposed {
            return Err(InventoryError::Disposed);
        }
        if process.owner_agent_session_id != state.owner {
            return Err(InventoryError::OwnerMismatch {
                actual: process.owner_agent_session_id,
                expected: state.owner.clone(),
            });
        }
        if state.processes.contains_key(&process.process_id)
            || state.completed.contains_key(&process.process_id)
        {
            return Err(InventoryError::AlreadyVisible(process.process_id));
        }
        state.revision = state.revision.wrapping_add(1);
        let yielded = YieldedShellProcess {
            process_id: process.process_id.clone(),
            name: process.name.clone(),
            inventory_instance_id: state.instance.clone(),
            visible_revision: state.revision,
            stdout_path: None,
            stderr_path: None,
            activity_kind: process.activity_kind,
        };
        state.processes.insert(process.process_id.clone(), process);
        state.publish(false);
        Ok(yielded)
    }

    pub fn complete(&self, process_id: &str, elapsed_milliseconds: Option<i64>) {
        let mut state = self.inner.lock().expect("process inventory lock");
        if state.disposed || state.processes.remove(process_id).is_none() {
            return;
        }
        state.completed.insert(
            process_id.into(),
            CompletedShellProcessState {
                process_id: process_id.into(),
                elapsed_milliseconds,
            },
        );
        state.revision = state.revision.wrapping_add(1);
        state.publish(false);
    }

    pub fn capture(&self) -> ShellProcessInventorySnapshot {
        self.inner.lock().expect("process inventory lock").capture()
    }

    pub fn subscribe(&self) -> InventorySubscription {
        let mut state = self.inner.lock().expect("process inventory lock");
        let id = state.next_subscriber;
        state.next_subscriber = state.next_subscriber.wrapping_add(1);
        let subscriber = Arc::new(Subscriber {
            slot: Mutex::new(Slot {
                pending: (!state.disposed).then(|| state.capture()),
                closed: state.disposed,
            }),
            changed: Notify::new(),
        });
        if !state.disposed {
            state.subscribers.insert(id, Arc::downgrade(&subscriber));
        }
        InventorySubscription {
            owner: Arc::downgrade(&self.inner),
            id,
            subscriber,
        }
    }

    pub fn dispose(&self) {
        let mut state = self.inner.lock().expect("process inventory lock");
        if state.disposed {
            return;
        }
        state.disposed = true;
        state.processes.clear();
        state.completed.clear();
        state.revision = state.revision.wrapping_add(1);
        state.publish(true);
    }
}

impl Drop for ShellProcessInventory {
    fn drop(&mut self) {
        self.dispose();
    }
}

struct Subscriber {
    slot: Mutex<Slot>,
    changed: Notify,
}

struct Slot {
    pending: Option<ShellProcessInventorySnapshot>,
    closed: bool,
}

pub struct InventorySubscription {
    owner: Weak<Mutex<InventoryState>>,
    id: u64,
    subscriber: Arc<Subscriber>,
}

impl InventorySubscription {
    /// Receives the latest pending snapshot. A closed subscription drains its
    /// last pending item before returning None, just like the source channel.
    pub async fn recv(&mut self) -> Option<ShellProcessInventorySnapshot> {
        loop {
            let changed = self.subscriber.changed.notified();
            {
                let mut slot = self
                    .subscriber
                    .slot
                    .lock()
                    .expect("inventory subscription lock");
                if let Some(snapshot) = slot.pending.take() {
                    return Some(snapshot);
                }
                if slot.closed {
                    return None;
                }
            }
            changed.await;
        }
    }

    pub fn try_recv(&mut self) -> Option<ShellProcessInventorySnapshot> {
        self.subscriber
            .slot
            .lock()
            .expect("inventory subscription lock")
            .pending
            .take()
    }

    pub fn is_closed(&self) -> bool {
        self.subscriber
            .slot
            .lock()
            .expect("inventory subscription lock")
            .closed
    }

    pub fn dispose(&self) {
        // Keep the same owner -> slot lock order as publication/disposal.
        if let Some(owner) = self.owner.upgrade() {
            owner
                .lock()
                .expect("process inventory lock")
                .subscribers
                .remove(&self.id);
        }
        self.subscriber
            .slot
            .lock()
            .expect("inventory subscription lock")
            .closed = true;
        self.subscriber.changed.notify_one();
    }
}

impl Drop for InventorySubscription {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    pub(super) fn active(id: &str) -> ActiveShellProcessState {
        ActiveShellProcessState {
            process_id: id.into(),
            name: "build".into(),
            command: "printf output".into(),
            description: "compile".into(),
            origin_tool_call_id: "call-id".into(),
            owner_agent_session_id: "agent-id".into(),
            owner_agent_name: "main".into(),
            parent_agent_session_id: "parent-id".into(),
            parent_agent_name: "parent".into(),
            depth: 1,
            started: Instant::now(),
            activity_kind: ActivityKind::Execution,
        }
    }

    #[test]
    fn owner_validation_unique_ids_tombstones_and_revision_match_source() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "inventory");
        let mut wrong_owner = active("wrong");
        wrong_owner.owner_agent_session_id = "other".into();
        assert_eq!(
            inventory.publish(wrong_owner),
            Err(InventoryError::OwnerMismatch {
                actual: "other".into(),
                expected: "agent-id".into()
            })
        );
        assert_eq!(inventory.capture().revision, 0);
        let yielded = inventory.publish(active("process")).unwrap();
        assert_eq!(yielded.visible_revision, 1);
        assert_eq!(yielded.inventory_instance_id, "inventory");
        assert_eq!(yielded.activity_kind, ActivityKind::Execution);
        assert!(yielded.stdout_path.is_none());
        assert!(yielded.stderr_path.is_none());
        assert!(matches!(
            inventory.publish(active("process")),
            Err(InventoryError::AlreadyVisible(_))
        ));
        inventory.complete("absent", Some(999));
        assert_eq!(inventory.capture().revision, 1);
        inventory.complete("process", None);
        inventory.complete("process", Some(123));
        let snapshot = inventory.capture();
        assert_eq!(snapshot.revision, 2);
        assert!(snapshot.processes.is_empty());
        assert_eq!(
            snapshot.completed_processes,
            vec![CompletedShellProcessState {
                process_id: "process".into(),
                elapsed_milliseconds: None
            }]
        );
        assert!(matches!(
            inventory.publish(active("process")),
            Err(InventoryError::AlreadyVisible(_))
        ));
    }

    #[test]
    fn snapshot_uses_utf16_ordinal_order_and_copies_records() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "inventory");
        // UTF-16 orders the surrogate pair before U+E000; UTF-8/scalar order differs.
        for id in ["\u{e000}", "\u{10000}", "active-b", "active-a"] {
            inventory.publish(active(id)).unwrap();
        }
        let mut snapshot = inventory.capture();
        assert_eq!(
            snapshot
                .processes
                .iter()
                .map(|p| p.process_id.as_str())
                .collect::<Vec<_>>(),
            ["active-a", "active-b", "\u{10000}", "\u{e000}"]
        );
        snapshot.processes[0].name = "modified copy".into();
        assert_eq!(inventory.capture().processes[0].name, "build");
        for id in ["\u{e000}", "\u{10000}"] {
            inventory.complete(id, Some(5_001));
        }
        assert_eq!(
            inventory
                .capture()
                .completed_processes
                .iter()
                .map(|p| p.process_id.as_str())
                .collect::<Vec<_>>(),
            ["\u{10000}", "\u{e000}"]
        );
    }

    #[tokio::test]
    async fn subscription_replays_initial_and_coalesces_to_latest_capacity_one() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "inventory");
        let mut initial = inventory.subscribe();
        assert_eq!(initial.recv().await.unwrap().revision, 0);
        for id in ["a", "b", "c"] {
            inventory.publish(active(id)).unwrap();
        }
        assert_eq!(initial.recv().await.unwrap().revision, 3);
        assert!(initial.try_recv().is_none());
        inventory.complete("b", Some(5_001));
        let mut late = inventory.subscribe();
        let replay = late.recv().await.unwrap();
        assert_eq!(replay.revision, 4);
        assert_eq!(
            replay.completed_processes[0].elapsed_milliseconds,
            Some(5_001)
        );
    }

    #[tokio::test]
    async fn disposal_publishes_final_removed_then_closes_existing_but_not_new_subscription() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "inventory");
        inventory.publish(active("a")).unwrap();
        let mut subscription = inventory.subscribe();
        inventory.dispose();
        inventory.dispose();
        let final_snapshot = subscription.recv().await.unwrap();
        assert!(final_snapshot.removed);
        assert_eq!(final_snapshot.revision, 2);
        assert!(final_snapshot.processes.is_empty());
        assert!(final_snapshot.completed_processes.is_empty());
        assert!(subscription.recv().await.is_none());
        let mut after_disposal = inventory.subscribe();
        assert!(after_disposal.recv().await.is_none());
        inventory.complete("a", None);
        assert_eq!(inventory.capture(), final_snapshot);
        assert_eq!(
            inventory.publish(active("late")),
            Err(InventoryError::Disposed)
        );
    }

    #[tokio::test]
    async fn unsubscribe_drains_pending_and_stops_future_updates() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "inventory");
        let mut subscription = inventory.subscribe();
        subscription.dispose();
        subscription.dispose();
        inventory.publish(active("later")).unwrap();
        assert_eq!(subscription.recv().await.unwrap().revision, 0);
        assert!(subscription.recv().await.is_none());
        assert!(inventory.inner.lock().unwrap().subscribers.is_empty());
    }

    #[tokio::test]
    async fn waiting_receiver_wakes_for_publish_and_dispose_without_lost_notification() {
        let inventory = Arc::new(ShellProcessInventory::with_instance_id(
            "agent-id",
            "inventory",
        ));
        let mut subscription = inventory.subscribe();
        subscription.recv().await.unwrap();
        let writer = inventory.clone();
        let publish = tokio::spawn(async move {
            tokio::task::yield_now().await;
            writer.publish(active("live")).unwrap();
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), subscription.recv())
                .await
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        publish.await.unwrap();
        let writer = inventory.clone();
        let dispose = tokio::spawn(async move {
            tokio::task::yield_now().await;
            writer.dispose();
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(1), subscription.recv())
                .await
                .unwrap()
                .unwrap()
                .removed
        );
        assert!(subscription.recv().await.is_none());
        dispose.await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_publish_unsubscribe_and_inventory_disposal_close_without_deadlock() {
        let inventory = ShellProcessInventory::with_instance_id("agent-id", "inventory");
        let mut subscription = inventory.subscribe();
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                for index in 0..64 {
                    match inventory.publish(active(&format!("process-{index}"))) {
                        Ok(_) => {}
                        Err(InventoryError::Disposed) => break,
                        Err(error) => panic!("unexpected publication error: {error}"),
                    }
                }
            });
            scope.spawn(|| {
                barrier.wait();
                inventory.dispose();
            });
            barrier.wait();
            subscription.dispose();
        });
        assert!(subscription.is_closed());
        // Depending on which close wins, the pending item is the last visible
        // update or the removed snapshot, but never a backlog.
        let _ = subscription.recv().await;
        assert!(subscription.recv().await.is_none());
        let snapshot = inventory.capture();
        assert!(snapshot.removed);
        assert!(snapshot.processes.is_empty());
        assert!(snapshot.completed_processes.is_empty());
        assert!(inventory.inner.lock().unwrap().subscribers.is_empty());
    }

    #[test]
    fn unique_instance_ids_use_existing_version7_identifier() {
        let first = ShellProcessInventory::new("agent").unwrap().capture();
        let second = ShellProcessInventory::new("agent").unwrap().capture();
        assert_ne!(first.inventory_instance_id, second.inventory_instance_id);
        let suffix = first
            .inventory_instance_id
            .strip_prefix("process-inventory-")
            .unwrap();
        assert_eq!(suffix.len(), 32);
        assert_eq!(&suffix[12..13], "7");
    }
}
