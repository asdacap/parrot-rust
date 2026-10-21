use super::support::*;
use parrot_domain::{contracts::async_ops::BoxFuture, protocol::messages::parrot::Event};
use parrot_runtime::children::*;
use parrot_storage::events::NeverCancelled;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub struct Publisher {
    pub name: &'static str,
    pub trace: Arc<Trace>,
    pub failures: AtomicUsize,
    pub gate: Option<Arc<Gate>>,
    pub captures: AtomicUsize,
}
impl SnapshotPublisher for Publisher {
    fn capture(&self) -> ChildResult<Vec<Event>> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Event {
            id: self.name.into(),
            ..Default::default()
        }])
    }
    fn start(&self) -> ChildResult<Operation<()>> {
        self.trace.push(format!("publish-start:{}", self.name));
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(ChildError::failure("snapshot", "PRIVATE-publisher-body"));
        }
        let (trace, name, gate) = (self.trace.clone(), self.name, self.gate.clone());
        Ok(Operation::start(async move {
            if let Some(gate) = gate {
                gate.wait(&NeverCancelled).await.unwrap();
            }
            trace.push(format!("publish-done:{name}"));
            Ok(())
        }))
    }
}
pub struct Resource {
    pub name: String,
    pub trace: Arc<Trace>,
    pub fail: bool,
    pub gate: Option<Arc<Gate>>,
}
impl Resource {
    pub fn new(name: &str, trace: Arc<Trace>, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            trace,
            fail,
            gate: None,
        })
    }
}
impl ScopeResource for Resource {
    type Error = ChildError;
    fn close(&self) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            if let Some(gate) = &self.gate {
                gate.wait(&NeverCancelled).await.unwrap();
            }
            self.trace.push(format!("resource:{}", self.name));
            if self.fail {
                Err(ChildError::failure("resource", self.name.clone()))
            } else {
                Ok(())
            }
        })
    }
}
pub struct Work {
    pub name: String,
    pub trace: Arc<Trace>,
    pub fail: bool,
}
impl Work {
    pub fn new(name: &str, trace: Arc<Trace>, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            trace,
            fail,
        })
    }
}
impl WorkOwner for Work {
    type Error = ChildError;
    fn settle(&self) -> BoxFuture<'_, ChildResult<()>> {
        Box::pin(async move {
            self.trace.push(format!("work:{}", self.name));
            if self.fail {
                Err(ChildError::failure("work", self.name.clone()))
            } else {
                Ok(())
            }
        })
    }
}
