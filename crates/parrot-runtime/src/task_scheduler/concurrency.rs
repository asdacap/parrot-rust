use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Session-scoped capacity for task runs beyond each owner's first run.
pub struct TaskConcurrencyBudget {
    state: Mutex<TaskConcurrencyState>,
    changed: watch::Sender<()>,
}
struct TaskConcurrencyState {
    limit: usize,
    used: usize,
}

impl TaskConcurrencyBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        let (changed, _) = watch::channel(());
        Arc::new(Self {
            state: Mutex::new(TaskConcurrencyState { limit, used: 0 }),
            changed,
        })
    }
    pub fn get_limit(&self) -> usize {
        self.state.lock().expect("task budget poisoned").limit
    }
    pub fn set_limit(&self, limit: usize) {
        self.state.lock().expect("task budget poisoned").limit = limit;
        self.changed.send_replace(());
    }
    pub(crate) fn owner(self: &Arc<Self>) -> Arc<TaskConcurrencyOwner> {
        Arc::new(TaskConcurrencyOwner {
            budget: self.clone(),
            running: Mutex::new(0),
        })
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }
}

pub(crate) struct TaskConcurrencyOwner {
    budget: Arc<TaskConcurrencyBudget>,
    running: Mutex<usize>,
}
impl TaskConcurrencyOwner {
    pub(crate) fn acquire(self: &Arc<Self>, recovering: bool) -> Option<TaskAdmission> {
        let mut running = self.running.lock().expect("task owner capacity poisoned");
        let mut state = self.budget.state.lock().expect("task budget poisoned");
        if *running != 0 {
            if !recovering && state.used >= state.limit {
                return None;
            }
            state.used += 1;
        }
        *running += 1;
        Some(TaskAdmission(self.clone()))
    }
}

pub(crate) struct TaskAdmission(Arc<TaskConcurrencyOwner>);
impl Drop for TaskAdmission {
    fn drop(&mut self) {
        let mut running = self.0.running.lock().expect("task owner capacity poisoned");
        let mut state = self.0.budget.state.lock().expect("task budget poisoned");
        if *running > 1 {
            state.used -= 1;
        }
        *running -= 1;
        drop(state);
        drop(running);
        self.0.budget.changed.send_replace(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn simultaneous_owners_share_only_the_extra_capacity() {
        let budget = TaskConcurrencyBudget::new(2);
        let barrier = Arc::new(Barrier::new(9));
        let threads = (0..8)
            .map(|_| {
                let budget = budget.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let owner = budget.owner();
                    let first = owner.acquire(false).unwrap();
                    barrier.wait();
                    let extra = owner.acquire(false);
                    let admitted = extra.is_some();
                    barrier.wait();
                    barrier.wait();
                    drop((first, extra));
                    admitted
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        barrier.wait();
        barrier.wait();
        let admitted = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 2);
        let owner = budget.owner();
        let admissions = (0..3)
            .map(|_| owner.acquire(false).unwrap())
            .collect::<Vec<_>>();
        assert!(owner.acquire(false).is_none());
        drop(admissions);
    }

    #[tokio::test]
    async fn free_allowance_moves_and_releases_are_retained_for_later_waits() {
        let budget = TaskConcurrencyBudget::new(1);
        let owner = budget.owner();
        let other = budget.owner();
        let mut changed = budget.subscribe();
        let first = owner.acquire(false).unwrap();
        let second = owner.acquire(false).unwrap();
        let other_first = other.acquire(false).unwrap();
        assert!(other.acquire(false).is_none());
        drop(first);
        changed.changed().await.unwrap();
        let other_extra = other.acquire(false).unwrap();
        drop((second, other_first));
        let reacquired_first = owner.acquire(false).unwrap();
        let reacquired_extra = owner.acquire(false).unwrap();
        assert!(other.acquire(false).is_none());
        drop(other_extra);
        let reacquired_other = other.acquire(false).unwrap();
        assert!(other.acquire(false).is_none());
        drop((reacquired_first, reacquired_extra, reacquired_other));
    }

    #[tokio::test]
    async fn changing_limit_retains_wakeups_and_never_revokes_held_admissions() {
        let budget = TaskConcurrencyBudget::new(2);
        let owner = budget.owner();
        let first = owner.acquire(false).unwrap();
        let second = owner.acquire(false).unwrap();
        let third = owner.acquire(false).unwrap();
        let mut changed = budget.subscribe();
        budget.set_limit(0);
        changed.changed().await.unwrap();
        assert_eq!(budget.get_limit(), 0);
        assert!(owner.acquire(false).is_none());
        let free = budget.owner().acquire(false).unwrap();
        drop(first);
        assert!(owner.acquire(false).is_none());
        drop(second);
        assert!(owner.acquire(false).is_none());
        budget.set_limit(1);
        assert_eq!(budget.get_limit(), 1);
        let extra = owner.acquire(false).unwrap();
        drop((third, extra, free));
        let first = owner.acquire(false).unwrap();
        let extra = owner.acquire(false).unwrap();
        assert!(owner.acquire(false).is_none());
        drop((first, extra));
    }

    #[test]
    fn grandfathered_usage_blocks_only_new_extras() {
        let budget = TaskConcurrencyBudget::new(0);
        let owner = budget.owner();
        let first = owner.acquire(true).unwrap();
        let second = owner.acquire(true).unwrap();
        assert!(owner.acquire(false).is_none());
        let another = budget.owner().acquire(false).unwrap();
        drop(first);
        assert!(owner.acquire(false).is_none());
        drop(second);
        assert!(owner.acquire(false).is_some());
        drop(another);
        budget.set_limit(1);
        let first = owner.acquire(false).unwrap();
        let extra = owner.acquire(false).unwrap();
        assert!(owner.acquire(false).is_none());
        drop((first, extra));
    }
}
