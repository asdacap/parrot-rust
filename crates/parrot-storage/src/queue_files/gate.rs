use std::sync::{Arc, Condvar, Mutex};

use parrot_domain::contracts::async_ops::Cancellation;
use tokio::sync::Notify;

use super::{Error, ErrorKind, Result};

#[derive(Default)]
struct State {
    held: bool,
    disposed: bool,
}

#[derive(Default)]
pub(super) struct Gate {
    state: Mutex<State>,
    changed: Condvar,
    notified: Notify,
}

pub(super) struct Permit(Arc<Gate>);

impl Gate {
    pub(super) fn acquire(self: &Arc<Self>, after_dispose: bool) -> Result<Permit> {
        let mut state = self.state.lock().expect("queue gate poisoned");
        if state.disposed && !after_dispose {
            return Err(Error::disposed());
        }
        while state.held {
            state = self.changed.wait(state).expect("queue gate poisoned");
        }
        state.held = true;
        Ok(Permit(self.clone()))
    }

    pub(super) fn try_acquire(self: &Arc<Self>) -> Result<Option<Permit>> {
        let mut state = self.state.lock().expect("queue gate poisoned");
        if state.disposed {
            return Err(Error::disposed());
        }
        if state.held {
            return Ok(None);
        }
        state.held = true;
        Ok(Some(Permit(self.clone())))
    }

    pub(super) async fn acquire_async(
        self: &Arc<Self>,
        cancellation: &dyn Cancellation,
    ) -> Result<Permit> {
        // Register before checking availability, so a release cannot be lost.
        let notified = self.notified.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let state = self.state.lock().expect("queue gate poisoned");
            if state.disposed {
                return Err(Error::disposed());
            }
        }
        if cancellation.is_cancelled() {
            return Err(Error::new(
                ErrorKind::CancelledBeforeWait,
                "operation cancelled",
            ));
        }
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::new(ErrorKind::Cancelled, "operation cancelled"));
            }
            {
                let mut state = self.state.lock().expect("queue gate poisoned");
                if !state.held {
                    state.held = true;
                    return Ok(Permit(self.clone()));
                }
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(Error::new(ErrorKind::Cancelled, "operation cancelled"));
                }
                () = &mut notified => {}
            }
            notified.set(self.notified.notified());
            notified.as_mut().enable();
        }
    }

    pub(super) fn dispose(&self) {
        self.state.lock().expect("queue gate poisoned").disposed = true;
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.state.lock().expect("queue gate poisoned").held = false;
        self.0.changed.notify_all();
        self.0.notified.notify_waiters();
    }
}
