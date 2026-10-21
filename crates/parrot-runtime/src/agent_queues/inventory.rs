use parrot_storage::queue_files::{Error, ErrorKind, Inventory, Owner, QueueState, Result};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::Notify;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueInventorySnapshot {
    pub owner_agent_session_id: String,
    pub inventory_instance_id: String,
    pub revision: u64,
    pub removed: bool,
    pub queues: Vec<QueueState>,
}

#[derive(Default)]
struct SlotState {
    pending: Option<QueueInventorySnapshot>,
    closed: bool,
}
#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    changed: Notify,
}
impl Slot {
    fn publish(&self, snapshot: QueueInventorySnapshot) {
        let mut state = self.state.lock().expect("inventory subscription poisoned");
        if !state.closed {
            state.pending = Some(snapshot);
            self.changed.notify_one();
        }
    }
    fn close(&self) {
        self.state
            .lock()
            .expect("inventory subscription poisoned")
            .closed = true;
        self.changed.notify_waiters();
    }
}
#[derive(Default)]
struct FeedState {
    subscribers: Vec<Weak<Slot>>,
    disposed: bool,
}
/// Capacity-one DropOldest feed: closing retains the final pending snapshot.
#[derive(Clone, Default)]
pub struct QueueInventoryFeed {
    state: Arc<Mutex<FeedState>>,
}
impl QueueInventoryFeed {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn subscribe(&self, initial: QueueInventorySnapshot) -> QueueInventorySubscription {
        let slot = Arc::new(Slot::default());
        let mut state = self.state.lock().expect("inventory feed poisoned");
        if state.disposed {
            slot.close();
        } else {
            state.subscribers.push(Arc::downgrade(&slot));
            slot.publish(initial);
        }
        QueueInventorySubscription {
            owner: Arc::downgrade(&self.state),
            slot,
        }
    }
    pub fn publish(&self, snapshot: QueueInventorySnapshot) {
        let targets = {
            let state = self.state.lock().expect("inventory feed poisoned");
            if state.disposed {
                Vec::new()
            } else {
                state.subscribers.iter().filter_map(Weak::upgrade).collect()
            }
        };
        for target in targets {
            target.publish(snapshot.clone());
        }
    }
    pub fn dispose(&self) {
        let targets = {
            let mut state = self.state.lock().expect("inventory feed poisoned");
            if state.disposed {
                return;
            }
            state.disposed = true;
            std::mem::take(&mut state.subscribers)
                .into_iter()
                .filter_map(|target| target.upgrade())
                .collect::<Vec<_>>()
        };
        for target in targets {
            target.close();
        }
    }
}

pub struct QueueInventorySubscription {
    owner: Weak<Mutex<FeedState>>,
    slot: Arc<Slot>,
}
impl QueueInventorySubscription {
    /// Exactly one consumer, corresponding to the source channel's SingleReader.
    pub async fn next(&mut self) -> Option<QueueInventorySnapshot> {
        loop {
            let changed = self.slot.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self
                    .slot
                    .state
                    .lock()
                    .expect("inventory subscription poisoned");
                if let Some(snapshot) = state.pending.take() {
                    return Some(snapshot);
                }
                if state.closed {
                    return None;
                }
            }
            changed.await;
        }
    }
    pub fn try_next(&mut self) -> Option<QueueInventorySnapshot> {
        self.slot
            .state
            .lock()
            .expect("inventory subscription poisoned")
            .pending
            .take()
    }
    pub fn dispose(&self) {
        if let Some(owner) = self.owner.upgrade() {
            owner
                .lock()
                .expect("inventory feed poisoned")
                .subscribers
                .retain(|slot| !Weak::ptr_eq(slot, &Arc::downgrade(&self.slot)));
        }
        self.slot.close();
    }
}
impl Drop for QueueInventorySubscription {
    fn drop(&mut self) {
        self.dispose();
    }
}

struct InventoryState {
    queues: BTreeMap<String, QueueState>,
    revision: u64,
    registered: bool,
    disposed: bool,
}
pub struct QueueInventory {
    owner: Owner,
    instance_id: String,
    state: Mutex<InventoryState>,
    feed: QueueInventoryFeed,
}
impl QueueInventory {
    /// Composition supplies the source-style `queue-inventory-<unique id>` identity.
    pub fn new(owner: Owner, instance_id: String) -> Result<Arc<Self>> {
        if instance_id.is_empty() {
            return Err(Error::new(
                ErrorKind::Argument,
                "inventory instance id must not be empty",
            ));
        }
        Ok(Arc::new(Self {
            owner,
            instance_id,
            state: Mutex::new(InventoryState {
                queues: BTreeMap::new(),
                revision: 0,
                registered: false,
                disposed: false,
            }),
            feed: QueueInventoryFeed::new(),
        }))
    }
    pub fn owner(&self) -> &Owner {
        &self.owner
    }
    fn capture_locked(&self, state: &InventoryState) -> QueueInventorySnapshot {
        QueueInventorySnapshot {
            owner_agent_session_id: self.owner.session_id.clone(),
            inventory_instance_id: self.instance_id.clone(),
            revision: state.revision,
            removed: state.disposed,
            queues: state.queues.values().cloned().collect(),
        }
    }
    fn publish_locked(&self, state: &mut InventoryState, changed: bool) {
        if changed {
            state.revision = state.revision.wrapping_add(1);
            self.feed.publish(self.capture_locked(state));
        }
    }
    pub fn capture(&self) -> QueueInventorySnapshot {
        self.capture_locked(&self.state.lock().expect("queue inventory poisoned"))
    }
    pub fn subscribe(&self) -> Result<QueueInventorySubscription> {
        let state = self.state.lock().expect("queue inventory poisoned");
        if state.disposed {
            return Err(Error::new(
                ErrorKind::Disposed,
                "Cannot access a disposed object.",
            ));
        }
        Ok(self.feed.subscribe(self.capture_locked(&state)))
    }
    pub fn dispose(&self) {
        let mut state = self.state.lock().expect("queue inventory poisoned");
        if state.disposed {
            return;
        }
        state.disposed = true;
        state.registered = false;
        state.queues.clear();
        state.revision = state.revision.wrapping_add(1);
        self.feed.publish(self.capture_locked(&state));
        self.feed.dispose();
    }
}
impl Inventory for QueueInventory {
    fn register_owner(&self, queues: &[QueueState]) -> Result<()> {
        let mut state = self.state.lock().expect("queue inventory poisoned");
        if state.disposed {
            return Err(Error::new(
                ErrorKind::Disposed,
                "Cannot access a disposed object.",
            ));
        }
        if state.registered {
            return Err(Error::new(
                ErrorKind::Inventory,
                format!(
                    "Queue owner '{}' is already registered.",
                    self.owner.session_id
                ),
            ));
        }
        state.registered = true;
        let mut changed = false;
        for queue in queues {
            if state.queues.contains_key(&queue.name) {
                return Err(Error::new(
                    ErrorKind::Argument,
                    "An item with the same key has already been added.",
                ));
            }
            let mut queue = queue.clone();
            queue.owner.session_id.clone_from(&self.owner.session_id);
            state.queues.insert(queue.name.clone(), queue);
            changed = true;
        }
        self.publish_locked(&mut state, changed);
        Ok(())
    }
    fn update(&self, queue: QueueState) -> Result<()> {
        if queue.owner.session_id.is_empty() {
            return Err(Error::new(
                ErrorKind::Argument,
                "owner agent session id must not be empty",
            ));
        }
        if queue.owner.session_id != self.owner.session_id {
            return Err(Error::new(
                ErrorKind::Inventory,
                format!(
                    "Queue owner '{}' does not match inventory owner '{}'.",
                    queue.owner.session_id, self.owner.session_id
                ),
            ));
        }
        let mut state = self.state.lock().expect("queue inventory poisoned");
        if state.disposed || !state.registered {
            return Ok(());
        }
        let changed = if queue.item_count > 0 {
            if state.queues.get(&queue.name) == Some(&queue) {
                false
            } else {
                state.queues.insert(queue.name.clone(), queue);
                true
            }
        } else {
            state.queues.remove(&queue.name).is_some()
        };
        self.publish_locked(&mut state, changed);
        Ok(())
    }
    fn unregister_owner(&self) -> Result<()> {
        let mut state = self.state.lock().expect("queue inventory poisoned");
        if state.disposed || !state.registered {
            return Ok(());
        }
        state.registered = false;
        let changed = !state.queues.is_empty();
        state.queues.clear();
        self.publish_locked(&mut state, changed);
        Ok(())
    }
}
