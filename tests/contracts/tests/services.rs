//! Deterministic service models, not OS/process/agent implementations.

use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OwnedWork, ScopeLifetime,
};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::runtime::AgentRuntime;
use parrot_domain::contracts::security::AccessSnapshot;

type Error = &'static str;
type Op<T> = Result<T, OperationError<Error>>;

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    let waker = Waker::from(Arc::new(NoopWake));
    future.poll(&mut Context::from_waker(&waker))
}

/// Deliberately fails if a test asks a ready-only executor to drive pending work.
fn ready<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    match poll(future.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("operation requires an explicit deterministic scheduler step"),
    }
}

#[derive(Default)]
struct Cancel(AtomicBool);
impl Cancel {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(poll_fn(|_| {
            if self.is_cancelled() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Scope {
    session: u32,
    agent: u32,
}

#[derive(Clone)]
struct Policy {
    scope: Scope,
    allowed: bool,
}
impl AccessSnapshot for Policy {
    type Error = Error;
    fn check_read(&self, read: &ResolvedRead) -> Result<(), Error> {
        self.check_write(read.actual_path())
    }
    fn check_write(&self, path: &Path) -> Result<(), Error> {
        if self.allowed && path == Path::new("/synthetic/service") {
            Ok(())
        } else {
            Err("policy denied actual target")
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct AgentId {
    caller: Scope,
    name: String,
}
struct Spawn {
    name: String,
    profile: &'static str,
}
struct SendMessage {
    target: AgentId,
    content: String,
}
struct Interrupt(AgentId);
struct Status(AgentId);
struct Wait(AgentId);
#[derive(Clone, Debug, Eq, PartialEq)]
struct Child {
    messages: Vec<String>,
    stopped: bool,
    captured_allowed: bool,
}
struct Runtime {
    caller: Scope,
    children: Mutex<BTreeMap<AgentId, Child>>,
}
impl Runtime {
    fn validate(&self, id: &AgentId) -> Result<(), Error> {
        if id.caller != self.caller {
            Err("outside caller scope")
        } else {
            Ok(())
        }
    }
}
impl AgentRuntime for Runtime {
    type Security = Policy;
    type SpawnRequest = Spawn;
    type SpawnResult = AgentId;
    type SendRequest = SendMessage;
    type SendResult = usize;
    type InterruptRequest = Interrupt;
    type InterruptResult = Child;
    type StatusRequest = Status;
    type StatusResult = Child;
    type WaitRequest = Wait;
    type WaitResult = Child;
    type Error = Error;

    fn spawn<'a>(
        &'a self,
        request: Spawn,
        security: Policy,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<AgentId>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if security.scope != self.caller
                || request.profile != "configured"
                || request.name.is_empty()
            {
                return Err(OperationError::Failed("wrong scope or unconfigured spawn"));
            }
            security
                .check_write(Path::new("/synthetic/service"))
                .map_err(OperationError::Failed)?;
            let id = AgentId {
                caller: self.caller.clone(),
                name: request.name,
            };
            let mut children = self.children.lock().unwrap();
            if children.contains_key(&id) {
                return Err(OperationError::Failed("duplicate child"));
            }
            children.insert(
                id.clone(),
                Child {
                    messages: Vec::new(),
                    stopped: false,
                    captured_allowed: security.allowed,
                },
            );
            Ok(id)
        })
    }
    fn send<'a>(
        &'a self,
        request: SendMessage,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<usize>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.validate(&request.target)
                .map_err(OperationError::Failed)?;
            let mut children = self.children.lock().unwrap();
            let child = children
                .get_mut(&request.target)
                .ok_or(OperationError::Failed("unknown child"))?;
            if child.stopped {
                return Err(OperationError::Failed("child stopped"));
            }
            child.messages.push(request.content);
            Ok(child.messages.len())
        })
    }
    fn interrupt<'a>(
        &'a self,
        request: Interrupt,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<Child>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            self.validate(&request.0).map_err(OperationError::Failed)?;
            let mut children = self.children.lock().unwrap();
            let child = children
                .get_mut(&request.0)
                .ok_or(OperationError::Failed("unknown child"))?;
            if child.stopped {
                return Err(OperationError::Failed("child already interrupted"));
            }
            child.stopped = true;
            Ok(child.clone())
        })
    }
    fn status(&self, request: Status) -> Result<Child, Error> {
        self.validate(&request.0)?;
        self.children
            .lock()
            .unwrap()
            .get(&request.0)
            .cloned()
            .ok_or("unknown child")
    }
    fn wait<'a>(
        &'a self,
        request: Wait,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<Child>> {
        Box::pin(poll_fn(move |_| {
            if cancellation.is_cancelled() {
                return Poll::Ready(Err(OperationError::Cancelled));
            }
            let child = match self.status(Status(request.0.clone())) {
                Ok(child) => child,
                Err(error) => return Poll::Ready(Err(OperationError::Failed(error))),
            };
            if child.stopped {
                Poll::Ready(Ok(child))
            } else {
                Poll::Pending
            }
        }))
    }
}

#[test]
fn runtime_typed_commands_enforce_caller_identity_and_owned_spawn_snapshot() {
    let caller = Scope {
        session: 1,
        agent: 2,
    };
    let runtime = Runtime {
        caller: caller.clone(),
        children: Mutex::new(BTreeMap::new()),
    };
    let mut policy = Policy {
        scope: caller.clone(),
        allowed: true,
    };
    let live = Cancel::default();
    let id = ready(runtime.spawn(
        Spawn {
            name: "child".into(),
            profile: "configured",
        },
        policy.clone(),
        &live,
    ))
    .unwrap();
    policy.allowed = false;
    assert!(runtime.status(Status(id.clone())).unwrap().captured_allowed);
    assert!(matches!(
        ready(runtime.spawn(
            Spawn {
                name: "denied".into(),
                profile: "configured"
            },
            policy,
            &live
        )),
        Err(OperationError::Failed(_))
    ));
    assert_eq!(
        ready(runtime.send(
            SendMessage {
                target: id.clone(),
                content: "message".into()
            },
            &live
        )),
        Ok(1)
    );
    assert_eq!(
        runtime.status(Status(id.clone())).unwrap().messages,
        vec!["message"]
    );
    let alien = AgentId {
        caller: Scope {
            session: 9,
            agent: 2,
        },
        name: "child".into(),
    };
    assert!(runtime.status(Status(alien.clone())).is_err());
    assert!(matches!(
        ready(runtime.send(
            SendMessage {
                target: alien,
                content: "not delivered".into()
            },
            &live
        )),
        Err(OperationError::Failed(_))
    ));
    let missing = AgentId {
        caller,
        name: "missing".into(),
    };
    assert!(runtime.status(Status(missing)).is_err());
    let cancelled = Cancel::default();
    let mut abandoned = runtime.wait(Wait(id.clone()), &cancelled);
    assert!(poll(abandoned.as_mut()).is_pending());
    cancelled.cancel();
    assert_eq!(
        poll(abandoned.as_mut()),
        Poll::Ready(Err(OperationError::Cancelled))
    );
    drop(abandoned);
    assert!(!runtime.status(Status(id.clone())).unwrap().stopped);
    let mut waiting = runtime.wait(Wait(id.clone()), &live);
    assert!(poll(waiting.as_mut()).is_pending());
    assert!(
        ready(runtime.interrupt(Interrupt(id.clone()), &live))
            .unwrap()
            .stopped
    );
    assert!(ready(waiting).unwrap().stopped);
    assert!(matches!(
        ready(runtime.send(
            SendMessage {
                target: id,
                content: "too late".into()
            },
            &live
        )),
        Err(OperationError::Failed(_))
    ));
}

struct CleanupState {
    completed: Vec<bool>,
    fail_at: usize,
    log: Vec<String>,
    closed: bool,
}
struct Lifetime(Mutex<CleanupState>);
impl OwnedWork for Lifetime {
    type Error = Error;
    fn settle(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            let mut failed = false;
            for index in 0..state.completed.len() {
                if !state.completed[index] {
                    state.log.push(format!("join:{index}"));
                    state.completed[index] = true;
                    if index == state.fail_at {
                        state.log.push(format!("error:{index}"));
                        failed = true;
                    }
                }
            }
            if failed {
                Err("cleanup failure")
            } else {
                Ok(())
            }
        })
    }
}
impl ScopeLifetime for Lifetime {
    type Error = Error;
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if self.0.lock().unwrap().closed {
                return Ok(());
            }
            let result = self.settle().await;
            let mut state = self.0.lock().unwrap();
            assert!(state.completed.iter().all(|completed| *completed));
            state.log.push("release:session".into());
            state.closed = true;
            result
        })
    }
}

#[test]
fn scoped_close_joins_all_cleanup_despite_failure_before_releasing_resources() {
    let lifetime = Lifetime(Mutex::new(CleanupState {
        completed: vec![false; 3],
        fail_at: 0,
        log: Vec::new(),
        closed: false,
    }));
    let unobserved = lifetime.close();
    drop(unobserved);
    assert!(!lifetime.0.lock().unwrap().closed);
    assert_eq!(ready(lifetime.close()), Err("cleanup failure"));
    let expected = vec!["join:0", "error:0", "join:1", "join:2", "release:session"];
    assert_eq!(lifetime.0.lock().unwrap().log, expected);
    ready(lifetime.close()).unwrap();
    assert_eq!(lifetime.0.lock().unwrap().log, expected);
}
