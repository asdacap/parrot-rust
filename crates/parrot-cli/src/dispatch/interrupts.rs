use std::sync::{Arc, Mutex};

pub trait InterruptListener: Send + Sync {
    fn interrupted(&self) -> bool;
}

pub struct CliInterruptListener<F: Fn() -> bool + Send + Sync>(pub F);
impl<F: Fn() -> bool + Send + Sync> InterruptListener for CliInterruptListener<F> {
    fn interrupted(&self) -> bool {
        (self.0)()
    }
}

/// SIGINT is offered to the single installed session listener. SIGTERM calls
/// `terminate` directly and cannot be claimed by a running turn.
pub struct Interrupts {
    listener: Mutex<Option<Arc<dyn InterruptListener>>>,
    stop: Arc<dyn Fn() + Send + Sync>,
}

impl Interrupts {
    pub fn new(stop: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            listener: Mutex::new(None),
            stop,
        }
    }

    pub fn install(&self, listener: Arc<dyn InterruptListener>) {
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
        if !listener.is_some_and(|listener| listener.interrupted()) {
            (self.stop)();
        }
    }

    pub fn terminate(&self) {
        (self.stop)();
    }
}
