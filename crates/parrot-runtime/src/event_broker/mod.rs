//! Session-owned, nonblocking event fanout and subscribe-before-replay handoff.
//!
//! Retained means a subscription's retention class, not persistence. The
//! publisher accepts [`Committed`] values, but never writes or commits them.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use parrot_domain::contracts::events::{
    EventPublisher, EventSubscription, RetentionClass, SubscriptionLimits,
};
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::Event;
use tokio::sync::Notify;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerError {
    InvalidCapacity,
    InvalidInventory,
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCapacity => "event retention capacities must be positive",
            Self::InvalidInventory => "Expected a complete tool snapshot batch of one inventory.",
        })
    }
}

impl std::error::Error for BrokerError {}

struct BufferedEvent {
    event: Event,
    retention: RetentionClass,
}

#[derive(Clone, Eq, PartialEq)]
struct InventoryKey {
    kind: String,
    owner: String,
}

struct InventoryBatch {
    key: InventoryKey,
    chunks: VecDeque<Event>,
}

fn inventory_key(event: &Event) -> Result<InventoryKey, BrokerError> {
    if !event.has_tool_snapshot() {
        return Err(BrokerError::InvalidInventory);
    }
    let snapshot = event.tool_snapshot();
    Ok(InventoryKey {
        kind: snapshot.kind.clone(),
        owner: snapshot.owner_agent_session_id.clone(),
    })
}

fn validate_inventory(chunks: &[Event]) -> Result<Option<InventoryKey>, BrokerError> {
    let Some(first) = chunks.first() else {
        return Ok(None);
    };
    let key = inventory_key(first)?;
    for event in &chunks[1..] {
        if inventory_key(event)? != key {
            return Err(BrokerError::InvalidInventory);
        }
    }
    Ok(Some(key))
}

#[derive(Default)]
struct Queue {
    events: VecDeque<BufferedEvent>,
    retained: usize,
    transient: usize,
    inventories: VecDeque<InventoryBatch>,
    active_inventory: VecDeque<Event>,
    prefer_event: bool,
    completed: bool,
}

impl Queue {
    fn publish(&mut self, event: Event, retention: RetentionClass, limits: SubscriptionLimits) {
        if self.completed {
            return;
        }
        let (count, capacity) = match retention {
            RetentionClass::Retained => (self.retained, limits.retained),
            RetentionClass::Transient => (self.transient, limits.transient),
        };
        if count == capacity {
            let index = self
                .events
                .iter()
                .position(|buffered| buffered.retention == retention)
                .expect("retention count matches buffered events");
            self.events.remove(index);
        } else {
            match retention {
                RetentionClass::Retained => self.retained += 1,
                RetentionClass::Transient => self.transient += 1,
            }
        }
        self.events.push_back(BufferedEvent { event, retention });
    }

    fn publish_inventory(&mut self, key: InventoryKey, chunks: &[Event]) {
        if self.completed {
            return;
        }
        if let Some(index) = self.inventories.iter().position(|batch| batch.key == key) {
            self.inventories.remove(index);
        }
        self.inventories.push_back(InventoryBatch {
            key,
            chunks: chunks.iter().cloned().collect(),
        });
    }

    fn pop(&mut self) -> Option<Event> {
        if self.active_inventory.is_empty() && (!self.prefer_event || self.events.is_empty()) {
            if let Some(batch) = self.inventories.pop_front() {
                self.active_inventory = batch.chunks;
            }
        }
        if let Some(event) = self.active_inventory.pop_front() {
            self.prefer_event = self.active_inventory.is_empty();
            return Some(event);
        }
        let buffered = self.events.pop_front()?;
        self.prefer_event = false;
        match buffered.retention {
            RetentionClass::Retained => self.retained -= 1,
            RetentionClass::Transient => self.transient -= 1,
        }
        Some(buffered.event)
    }
}

struct SubscriptionInner {
    queue: Mutex<Queue>,
    changed: Notify,
    limits: SubscriptionLimits,
}

impl SubscriptionInner {
    fn complete(&self) {
        lock(&self.queue).completed = true;
        self.changed.notify_waiters();
    }
}

#[derive(Default)]
struct BrokerState {
    subscriptions: Vec<Weak<SubscriptionInner>>,
    completed: bool,
}

/// Fanout does not replay events published before subscription. Each listener
/// has its own two lossy budgets; inventory batches are kept separately and
/// coalesce only while pending, by exact kind/owner identity.
pub struct EventBroker {
    state: Mutex<BrokerState>,
    limits: SubscriptionLimits,
}

impl Default for EventBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBroker {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BrokerState::default()),
            limits: SubscriptionLimits::default(),
        }
    }

    pub fn with_limits(limits: SubscriptionLimits) -> Result<Self, BrokerError> {
        if limits.retained == 0 || limits.transient == 0 {
            return Err(BrokerError::InvalidCapacity);
        }
        Ok(Self {
            state: Mutex::new(BrokerState::default()),
            limits,
        })
    }

    /// Eagerly admits the listener before returning, including before its first
    /// read. Subscribing after broker completion returns a completed listener.
    pub fn subscribe(&self) -> Subscription {
        let inner = Arc::new(SubscriptionInner {
            queue: Mutex::new(Queue::default()),
            changed: Notify::new(),
            limits: self.limits,
        });
        let mut state = lock(&self.state);
        if state.completed {
            inner.complete();
        } else {
            state.subscriptions.retain(|entry| entry.strong_count() > 0);
            state.subscriptions.push(Arc::downgrade(&inner));
        }
        Subscription { inner }
    }

    /// Subscribes before reading persistence. `None` disables replay, whereas
    /// `Some("")` still requests a real replay. Current snapshots are captured
    /// only when the caller has consumed the persisted suffix, then live reads
    /// begin. This preserves snapshot freshness for a slow replay consumer.
    ///
    /// The caller supplies actual storage and registry/usage operations. A
    /// failed replay load or dropping this future releases its eager listener;
    /// an initial-capture failure is observed after any replay already yielded.
    /// This does not own session mutations.
    pub async fn listen<R, RF, I, IF, E>(
        &self,
        cursor: Option<&str>,
        load_replay: R,
        capture_initial: I,
    ) -> Result<ListeningSubscription<I, IF>, E>
    where
        R: FnOnce() -> RF,
        RF: Future<Output = Result<Vec<Event>, E>>,
        I: FnOnce() -> IF,
        IF: Future<Output = Result<Vec<Event>, E>>,
    {
        let subscription = self.subscribe();
        let replay = match cursor {
            Some(cursor) => subscription.replay_after(cursor, load_replay().await?),
            None => subscription.replay_after("", Vec::new()),
        };
        Ok(ListeningSubscription {
            replay,
            initial: tokio::sync::Mutex::new(InitialCapture {
                capture: Some(capture_initial),
                pending: None,
                events: VecDeque::new(),
                failed: false,
            }),
        })
    }

    fn publish(&self, event: &Event, retention: RetentionClass) {
        let mut state = lock(&self.state);
        if state.completed {
            return;
        }
        state.subscriptions.retain(|entry| {
            let Some(target) = entry.upgrade() else {
                return false;
            };
            lock(&target.queue).publish(event.clone(), retention, target.limits);
            target.changed.notify_waiters();
            true
        });
    }

    /// Validates even when no subscriber exists or the broker is completed.
    /// Empty batches are no-ops. Chunk numbering/revisions are not rewritten or
    /// further constrained: the source validates only payload kind and owner.
    pub fn publish_inventory(&self, chunks: &[Event]) -> Result<(), BrokerError> {
        let Some(key) = validate_inventory(chunks)? else {
            return Ok(());
        };
        let mut state = lock(&self.state);
        if state.completed {
            return Ok(());
        }
        state.subscriptions.retain(|entry| {
            let Some(target) = entry.upgrade() else {
                return false;
            };
            lock(&target.queue).publish_inventory(key.clone(), chunks);
            target.changed.notify_waiters();
            true
        });
        Ok(())
    }

    /// Publishes an already-retained runtime event through the retained queue.
    /// Retention is a fanout policy only: it does not persist this value or make
    /// it eligible for storage replay. Callers must use `publish_committed` only
    /// after proving an actual durable commit.
    pub fn publish_retained(&self, event: &Event) {
        self.publish(event, RetentionClass::Retained);
    }

    /// Stops future publication and completes every listener without discarding
    /// its queued events. A foreground TurnEnded is just another event.
    pub fn complete(&self) {
        let mut state = lock(&self.state);
        if state.completed {
            return;
        }
        state.completed = true;
        for target in state
            .subscriptions
            .drain(..)
            .filter_map(|entry| entry.upgrade())
        {
            target.complete();
        }
    }
}

impl Drop for EventBroker {
    fn drop(&mut self) {
        self.complete();
    }
}

impl EventPublisher for EventBroker {
    type Event = Event;
    type Error = BrokerError;

    fn publish_committed<'a>(
        &'a self,
        event: &'a Committed<Event>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.publish(event.value(), RetentionClass::Retained);
            Ok(())
        })
    }

    fn publish_transient<'a>(
        &'a self,
        event: Event,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<BrokerError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.publish(&event, RetentionClass::Transient);
            Ok(())
        })
    }
}

/// A listener owns its queue. Dropping it unregisters it without retaining a
/// dead queue in the broker. Explicit completion preserves queued data.
pub struct Subscription {
    inner: Arc<SubscriptionInner>,
}

impl Subscription {
    pub fn try_next(&self) -> Option<Event> {
        lock(&self.inner.queue).pop()
    }

    pub fn publish_inventory(&self, chunks: &[Event]) -> Result<(), BrokerError> {
        let Some(key) = validate_inventory(chunks)? else {
            return Ok(());
        };
        lock(&self.inner.queue).publish_inventory(key, chunks);
        self.inner.changed.notify_waiters();
        Ok(())
    }

    pub fn complete(&self) {
        self.inner.complete();
    }

    async fn next_event(
        &self,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Option<Event>, BrokerError> {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut queue = lock(&self.inner.queue);
                if let Some(event) = queue.pop() {
                    return Ok(Some(event));
                }
                if queue.completed {
                    return Ok(None);
                }
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(OperationError::Cancelled),
                _ = changed => {}
            }
        }
    }

    /// Call only after loading the actual persisted snapshot *after* this
    /// eager subscription was created. Unknown cursors replay the whole
    /// snapshot; known cursors resume after their first occurrence.
    ///
    /// Raw replay duplicates are preserved. Only the first overlapping live
    /// copy of each replayed id is suppressed, matching UserSession.Listen.
    pub fn replay_after(self, cursor: &str, persisted: Vec<Event>) -> ReplaySubscription {
        let skip = persisted
            .iter()
            .position(|event| event.id == cursor)
            .map_or(0, |index| index + 1);
        let events: VecDeque<_> = persisted.into_iter().skip(skip).collect();
        let replayed = events.iter().map(|event| event.id.clone()).collect();
        ReplaySubscription {
            live: self,
            replay: Mutex::new(ReplayState { events, replayed }),
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.complete();
    }
}

impl EventSubscription for Subscription {
    type Event = Event;
    type Error = BrokerError;

    fn next<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Event>, BrokerError>> {
        Box::pin(self.next_event(cancellation))
    }

    fn complete(&self) -> Result<(), BrokerError> {
        self.complete();
        Ok(())
    }
}

struct ReplayState {
    events: VecDeque<Event>,
    replayed: HashSet<String>,
}

/// Finite persisted replay followed by the already-admitted live listener.
/// Storage loading, initial scope snapshots and session lifetime are owned by
/// the caller; this component does not create a persistence/security authority.
pub struct ReplaySubscription {
    live: Subscription,
    replay: Mutex<ReplayState>,
}

impl EventSubscription for ReplaySubscription {
    type Event = Event;
    type Error = BrokerError;

    fn next<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Event>, BrokerError>> {
        Box::pin(async move {
            if let Some(event) = lock(&self.replay).events.pop_front() {
                return Ok(Some(event));
            }
            loop {
                let Some(event) = self.live.next_event(cancellation).await? else {
                    return Ok(None);
                };
                if !lock(&self.replay).replayed.remove(&event.id) {
                    return Ok(Some(event));
                }
            }
        })
    }

    fn complete(&self) -> Result<(), BrokerError> {
        self.live.complete();
        Ok(())
    }
}

/// Read failures keep the caller's initial-capture error distinct from broker
/// cancellation. Persisted replay already delivered is never taken back.
#[derive(Debug, Eq, PartialEq)]
pub enum ListenError<E> {
    Initial(E),
    Broker(BrokerError),
}

impl<E: fmt::Display> fmt::Display for ListenError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initial(error) => error.fmt(formatter),
            Self::Broker(error) => error.fmt(formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ListenError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Initial(error) => Some(error),
            Self::Broker(error) => Some(error),
        }
    }
}

struct InitialCapture<I, IF> {
    capture: Option<I>,
    pending: Option<Pin<Box<IF>>>,
    events: VecDeque<Event>,
    failed: bool,
}

/// Source-phase replay, then a once-only lazy snapshot capture, then live data.
/// The pending initial read belongs to this listener, not an individual `next`
/// observation, so dropping one observation cannot skip or restart capture.
pub struct ListeningSubscription<I, IF> {
    replay: ReplaySubscription,
    initial: tokio::sync::Mutex<InitialCapture<I, IF>>,
}

impl<I, IF, E> EventSubscription for ListeningSubscription<I, IF>
where
    I: FnOnce() -> IF + Send,
    IF: Future<Output = Result<Vec<Event>, E>> + Send,
    E: Send + Sync + 'static,
{
    type Event = Event;
    type Error = ListenError<E>;

    fn next<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Event>, ListenError<E>>> {
        Box::pin(async move {
            let mut initial = tokio::select! {
                biased;
                initial = self.initial.lock() => initial,
                _ = cancellation.cancelled() => return Err(OperationError::Cancelled),
            };
            if let Some(event) = lock(&self.replay.replay).events.pop_front() {
                return Ok(Some(event));
            }
            if initial.failed {
                return Ok(None);
            }
            if let Some(capture) = initial.capture.take() {
                initial.pending = Some(Box::pin(capture()));
            }
            if let Some(pending) = initial.pending.as_mut() {
                let result = tokio::select! {
                    biased;
                    result = pending => result,
                    _ = cancellation.cancelled() => return Err(OperationError::Cancelled),
                };
                initial.pending = None;
                match result {
                    Ok(events) => initial.events.extend(events),
                    Err(error) => {
                        initial.failed = true;
                        self.replay.live.complete();
                        return Err(OperationError::Failed(ListenError::Initial(error)));
                    }
                }
            }
            if let Some(event) = initial.events.pop_front() {
                return Ok(Some(event));
            }
            self.replay
                .next(cancellation)
                .await
                .map_err(|error| match error {
                    OperationError::Cancelled => OperationError::Cancelled,
                    OperationError::Failed(error) => {
                        OperationError::Failed(ListenError::Broker(error))
                    }
                })
        })
    }

    fn complete(&self) -> Result<(), ListenError<E>> {
        self.replay.complete().map_err(ListenError::Broker)
    }
}
