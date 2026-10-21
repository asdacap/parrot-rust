use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use parrot_domain::contracts::async_ops::{Cancellation, Clock, OperationError};
use tokio::sync::Notify;

use super::{InteractionError, InteractionErrorKind};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) struct Entry<I, O> {
    pub input: I,
    pub started: Instant,
    outcome: Mutex<Option<Result<O, InteractionError>>>,
    changed: Notify,
}

pub(crate) struct Store<I, O> {
    state: Mutex<State<I, O>>,
}

struct State<I, O> {
    disposed: bool,
    entries: BTreeMap<String, Arc<Entry<I, O>>>,
}

impl<I, O: Clone> Store<I, O> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                disposed: false,
                entries: BTreeMap::new(),
            }),
        }
    }

    pub fn check_open(&self) -> Result<(), InteractionError> {
        if lock(&self.state).disposed {
            Err(InteractionError::new(
                InteractionErrorKind::Disposed,
                "session disposed",
            ))
        } else {
            Ok(())
        }
    }

    pub fn insert(
        &self,
        id: String,
        input: I,
        started: Instant,
    ) -> Result<Arc<Entry<I, O>>, InteractionError> {
        self.insert_with(id, input, started, |_| Ok(()))
    }

    pub fn insert_with(
        &self,
        id: String,
        input: I,
        started: Instant,
        publish: impl FnOnce(&I) -> Result<(), InteractionError>,
    ) -> Result<Arc<Entry<I, O>>, InteractionError> {
        let mut state = lock(&self.state);
        if state.disposed {
            return Err(InteractionError::new(
                InteractionErrorKind::Disposed,
                "session disposed",
            ));
        }
        if state.entries.contains_key(&id) {
            return Err(InteractionError::new(
                InteractionErrorKind::Identifier,
                "duplicate request identifier",
            ));
        }
        let entry = Arc::new(Entry {
            input,
            started,
            outcome: Mutex::new(None),
            changed: Notify::new(),
        });
        state.entries.insert(id.clone(), Arc::clone(&entry));
        if let Err(error) = publish(&entry.input) {
            state.entries.remove(&id);
            return Err(error);
        }
        Ok(entry)
    }

    pub fn snapshot<R>(&self, mut describe: impl FnMut(&str, &Entry<I, O>) -> R) -> Vec<R> {
        lock(&self.state)
            .entries
            .iter()
            .map(|(id, entry)| describe(id, entry))
            .collect()
    }

    pub fn settle(
        &self,
        id: &str,
        not_found: InteractionError,
        reply: impl FnOnce(&I) -> Result<O, InteractionError>,
    ) -> Result<(), InteractionError> {
        let mut state = lock(&self.state);
        let entry = state.entries.get(id).ok_or(not_found)?;
        let result = reply(&entry.input)?;
        *lock(&entry.outcome) = Some(Ok(result));
        entry.changed.notify_waiters();
        state.entries.remove(id);
        Ok(())
    }

    pub fn fail(
        &self,
        id: &str,
        not_found: InteractionError,
        failure: InteractionError,
    ) -> Result<(), InteractionError> {
        let mut state = lock(&self.state);
        let entry = state.entries.get(id).ok_or(not_found)?;
        *lock(&entry.outcome) = Some(Err(failure));
        entry.changed.notify_waiters();
        state.entries.remove(id);
        Ok(())
    }

    pub fn remove(&self, id: &str, expected: &Arc<Entry<I, O>>) -> bool {
        let mut state = lock(&self.state);
        if state
            .entries
            .get(id)
            .is_some_and(|entry| Arc::ptr_eq(entry, expected))
        {
            state.entries.remove(id);
            true
        } else {
            false
        }
    }

    pub fn close(&self, failure: InteractionError) {
        let mut state = lock(&self.state);
        if state.disposed {
            return;
        }
        state.disposed = true;
        for entry in state.entries.values() {
            *lock(&entry.outcome) = Some(Err(failure.clone()));
            entry.changed.notify_waiters();
        }
        state.entries.clear();
    }

    pub fn withdraw_on_drop<'a>(
        &'a self,
        id: &'a str,
        entry: &'a Arc<Entry<I, O>>,
    ) -> Withdraw<'a, I, O> {
        Withdraw {
            store: self,
            id,
            entry,
        }
    }

    pub async fn wait(
        &self,
        id: &str,
        entry: Arc<Entry<I, O>>,
        cancellation: &dyn Cancellation,
        deadline: Option<Instant>,
        clock: &dyn Clock,
        away: O,
    ) -> Result<O, OperationError<InteractionError>> {
        let _withdraw = Withdraw {
            store: self,
            id,
            entry: &entry,
        };
        loop {
            let changed = entry.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(outcome) = lock(&entry.outcome).clone() {
                return outcome.map_err(OperationError::Failed);
            }
            let sleep = async {
                match deadline {
                    Some(deadline) => {
                        let _ = clock.sleep_until(deadline, cancellation).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            let cancelled = tokio::select! {
                biased;
                _ = &mut changed => continue,
                _ = cancellation.cancelled() => true,
                _ = sleep => cancellation.is_cancelled(),
            };
            if self.remove(id, &entry) {
                return if cancelled {
                    Err(OperationError::Cancelled)
                } else {
                    Ok(away)
                };
            }
        }
    }
}

pub(crate) struct Withdraw<'a, I, O: Clone> {
    store: &'a Store<I, O>,
    id: &'a str,
    entry: &'a Arc<Entry<I, O>>,
}

impl<I, O: Clone> Drop for Withdraw<'_, I, O> {
    fn drop(&mut self) {
        self.store.remove(self.id, self.entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_publication_guard_does_not_remove_reused_identity() {
        let store = Store::<(), ()>::new();
        let first = store.insert("id".into(), (), Instant::now()).unwrap();
        let guard = store.withdraw_on_drop("id", &first);
        assert!(store.remove("id", &first));
        let second = store.insert("id".into(), (), Instant::now()).unwrap();
        drop(guard);
        assert_eq!(store.snapshot(|id, _| id.to_owned()), ["id"]);
        assert!(store.remove("id", &second));
    }
}
