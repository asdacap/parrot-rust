use std::sync::{Arc, Mutex};

/// SIGINT is offered to the single installed session listener. SIGTERM calls
/// `terminate` directly and cannot be claimed by a running turn.
pub struct Interrupts {
    listener: Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
    stop: Arc<dyn Fn() + Send + Sync>,
}

impl Interrupts {
    pub fn new(stop: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            listener: Mutex::new(None),
            stop,
        }
    }

    pub fn install(&self, listener: Arc<dyn Fn() -> bool + Send + Sync>) {
        *self.listener.lock().expect("interrupt listener poisoned") = Some(listener);
    }

    pub fn remove(&self) {
        *self.listener.lock().expect("interrupt listener poisoned") = None;
    }

    pub fn signal(&self) {
        let listener = self
            .listener
            .lock()
            .expect("interrupt listener poisoned")
            .clone();
        if !listener.is_some_and(|listener| listener()) {
            (self.stop)();
        }
    }

    pub fn terminate(&self) {
        (self.stop)();
    }
}
