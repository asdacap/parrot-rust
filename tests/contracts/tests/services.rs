//! Deterministic service models, not OS/process/agent implementations.

use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OwnedWork, ScopeLifetime,
};
use parrot_domain::contracts::interactions::{PermissionBroker, QuestionBroker};
use parrot_domain::contracts::paths::ResolvedRead;
use parrot_domain::contracts::processes::{ManagedProcess, ProcessOwner};
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProcessState {
    stdin: String,
    yields: Vec<Duration>,
    stopped: bool,
    joined: bool,
}

#[derive(Clone)]
struct Process {
    owner: Scope,
    state: Arc<Mutex<ProcessState>>,
}
impl ManagedProcess for Process {
    type Owner = Scope;
    type Signal = Stop;
    type WaitResult = ProcessState;
    type Snapshot = ProcessState;
    type Error = Error;

    fn owner(&self) -> &Scope {
        &self.owner
    }
    fn snapshot(&self) -> Result<ProcessState, Error> {
        Ok(self.state.lock().unwrap().clone())
    }
    fn write_stdin<'a>(
        &'a self,
        input: String,
        yield_after: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<ProcessState>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            let mut state = self.state.lock().unwrap();
            if state.stopped {
                return Err(OperationError::Failed("process already stopped"));
            }
            state.stdin.push_str(&input);
            state.yields.push(yield_after);
            Ok(state.clone())
        })
    }
    fn signal(&self, _signal: Stop) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        if state.stopped {
            return Err("process already stopped");
        }
        state.stopped = true;
        Ok(())
    }
    fn wait<'a>(&'a self, cancellation: &'a dyn Cancellation) -> BoxFuture<'a, Op<ProcessState>> {
        Box::pin(poll_fn(move |_| {
            if cancellation.is_cancelled() {
                return Poll::Ready(Err(OperationError::Cancelled));
            }
            let state = self.state.lock().unwrap();
            if state.stopped {
                Poll::Ready(Ok(state.clone()))
            } else {
                Poll::Pending
            }
        }))
    }
    fn settle(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.stopped = true;
            state.joined = true;
            Ok(())
        })
    }
}
struct Stop;

struct Start {
    program: &'static str,
}
struct Processes {
    scope: Scope,
    owned: Mutex<Vec<Process>>,
}
impl ProcessOwner for Processes {
    type StartRequest = Start;
    type Security = Policy;
    type Process = Process;
    type Error = Error;

    fn start<'a>(
        &'a self,
        request: Start,
        security: Policy,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<Process>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            if security.scope != self.scope || request.program != "configured" {
                return Err(OperationError::Failed(
                    "wrong scope or unconfigured executable",
                ));
            }
            security
                .check_write(Path::new("/synthetic/service"))
                .map_err(OperationError::Failed)?;
            let process = Process {
                owner: self.scope.clone(),
                state: Arc::new(Mutex::new(ProcessState::default())),
            };
            self.owned.lock().unwrap().push(process.clone());
            Ok(process)
        })
    }
    fn settle(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let owned = self.owned.lock().unwrap().clone();
            let mut failure = None;
            for process in owned {
                if let Err(error) = process.settle().await {
                    failure.get_or_insert(error);
                }
            }
            failure.map_or(Ok(()), Err)
        })
    }
}

#[test]
fn process_observation_cancellation_and_drop_preserve_exact_owner_until_settle() {
    let scope = Scope {
        session: 7,
        agent: 1,
    };
    let owner = Processes {
        scope: scope.clone(),
        owned: Mutex::new(Vec::new()),
    };
    let policy = Policy {
        scope: scope.clone(),
        allowed: true,
    };
    let live = Cancel::default();
    let process = ready(owner.start(
        Start {
            program: "configured",
        },
        policy.clone(),
        &live,
    ))
    .unwrap();
    assert_eq!(process.owner(), &scope);
    let snapshot =
        ready(process.write_stdin("input".into(), Duration::from_millis(8), &live)).unwrap();
    assert_eq!(snapshot.stdin, "input");
    assert_eq!(snapshot.yields, vec![Duration::from_millis(8)]);
    let cancelled = Cancel::default();
    let mut wait = process.wait(&cancelled);
    assert!(poll(wait.as_mut()).is_pending());
    cancelled.cancel();
    assert_eq!(
        poll(wait.as_mut()),
        Poll::Ready(Err(OperationError::Cancelled))
    );
    drop(wait);
    assert!(!process.snapshot().unwrap().stopped);
    assert!(matches!(
        ready(process.write_stdin("forbidden".into(), Duration::ZERO, &cancelled)),
        Err(OperationError::Cancelled)
    ));
    assert_eq!(process.snapshot().unwrap().stdin, "input");
    process.signal(Stop).unwrap();
    assert!(process.signal(Stop).is_err());
    assert!(ready(process.wait(&live)).unwrap().stopped);
    let second = ready(owner.start(
        Start {
            program: "configured",
        },
        policy.clone(),
        &live,
    ))
    .unwrap();
    let second_state = second.state.clone();
    drop(second);
    assert_eq!(owner.owned.lock().unwrap().len(), 2);
    assert!(matches!(
        ready(owner.start(Start { program: "unknown" }, policy.clone(), &live)),
        Err(OperationError::Failed(_))
    ));
    assert!(matches!(
        ready(owner.start(
            Start {
                program: "configured"
            },
            Policy {
                allowed: false,
                ..policy
            },
            &live
        )),
        Err(OperationError::Failed(_))
    ));
    ready(owner.settle()).unwrap();
    assert!(process.snapshot().unwrap().joined);
    assert!(second_state.lock().unwrap().joined);
    ready(owner.settle()).unwrap();
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RequestId {
    session: u32,
    number: u32,
}
struct Request {
    id: RequestId,
    prompt: String,
}
#[derive(Clone)]
enum Answer {
    Waiting,
    Replied(Result<String, Error>),
}
struct Broker {
    session: u32,
    requests: Mutex<BTreeMap<RequestId, Answer>>,
}
impl Broker {
    fn new(session: u32) -> Self {
        Self {
            session,
            requests: Mutex::new(BTreeMap::new()),
        }
    }
    fn ask<'a>(
        &'a self,
        request: Request,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<String>> {
        let mut registered = false;
        Box::pin(poll_fn(move |_| {
            if request.id.session != self.session || request.prompt.is_empty() {
                return Poll::Ready(Err(OperationError::Failed(
                    "wrong session or empty request",
                )));
            }
            let mut requests = self.requests.lock().unwrap();
            if cancellation.is_cancelled() {
                if registered {
                    requests.remove(&request.id);
                }
                return Poll::Ready(Err(OperationError::Cancelled));
            }
            if !registered {
                if requests.contains_key(&request.id) {
                    return Poll::Ready(Err(OperationError::Failed("duplicate pending id")));
                }
                requests.insert(request.id.clone(), Answer::Waiting);
                registered = true;
            }
            match requests.get(&request.id).cloned() {
                Some(Answer::Waiting) => Poll::Pending,
                Some(Answer::Replied(reply)) => {
                    requests.remove(&request.id);
                    Poll::Ready(reply.map_err(OperationError::Failed))
                }
                None => Poll::Ready(Err(OperationError::Failed("request disappeared"))),
            }
        }))
    }
    fn pending(&self) -> Result<Vec<RequestId>, Error> {
        Ok(self
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, state)| matches!(state, Answer::Waiting).then_some(id.clone()))
            .collect())
    }
    fn finish(&self, id: &RequestId, reply: Result<String, Error>) -> Result<(), Error> {
        if id.session != self.session {
            return Err("wrong session");
        }
        let mut requests = self.requests.lock().unwrap();
        match requests.get_mut(id) {
            Some(answer @ Answer::Waiting) => {
                *answer = Answer::Replied(reply);
                Ok(())
            }
            _ => Err("wrong or stale request"),
        }
    }
}
struct Questions(Broker);
struct Permissions(Broker);

macro_rules! broker_contract {
    ($wrapper:ident, $contract:ident, $method:ident) => {
        impl $contract for $wrapper {
            type Request = Request;
            type Reply = String;
            type Pending = Vec<RequestId>;
            type RequestId = RequestId;
            type Error = Error;
            fn $method<'a>(
                &'a self,
                request: Request,
                cancellation: &'a dyn Cancellation,
            ) -> BoxFuture<'a, Op<String>> {
                self.0.ask(request, cancellation)
            }
            fn pending(&self) -> Result<Vec<RequestId>, Error> {
                self.0.pending()
            }
            fn reply(&self, id: &RequestId, reply: String) -> Result<(), Error> {
                self.0.finish(id, Ok(reply))
            }
            fn reject(&self, id: &RequestId) -> Result<(), Error> {
                self.0.finish(id, Err("rejected"))
            }
            fn withdraw(&self, id: &RequestId) -> Result<(), Error> {
                self.0.finish(id, Err("withdrawn"))
            }
        }
    };
}
broker_contract!(Questions, QuestionBroker, ask);
impl PermissionBroker for Permissions {
    type Request = Request;
    type Reply = String;
    type Pending = Vec<RequestId>;
    type RequestId = RequestId;
    type Error = Error;
    fn request<'a>(
        &'a self,
        request: Request,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Op<String>> {
        self.0.ask(request, cancellation)
    }
    fn pending(&self) -> BoxFuture<'_, Result<Vec<RequestId>, Error>> {
        Box::pin(async { self.0.pending() })
    }
    fn reply<'a>(&'a self, id: &'a RequestId, reply: String) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move { self.0.finish(id, Ok(reply)) })
    }
    fn reject<'a>(&'a self, id: &'a RequestId) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move { self.0.finish(id, Err("rejected")) })
    }
    fn withdraw<'a>(&'a self, id: &'a RequestId) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move { self.0.finish(id, Err("withdrawn")) })
    }
}

fn request(session: u32, number: u32) -> Request {
    Request {
        id: RequestId { session, number },
        prompt: "configured prompt".into(),
    }
}

#[test]
fn questions_are_session_scoped_and_wrong_stale_reject_withdraw_are_explicit() {
    let broker = Questions(Broker::new(10));
    let other = Questions(Broker::new(11));
    let live = Cancel::default();
    let id = request(10, 1).id;
    let mut waiting = broker.ask(request(10, 1), &live);
    assert!(poll(waiting.as_mut()).is_pending());
    assert_eq!(broker.pending().unwrap(), vec![id.clone()]);
    assert!(other.pending().unwrap().is_empty());
    assert!(other.reply(&id, "wrong scope".into()).is_err());
    assert!(
        broker
            .reply(&request(11, 1).id, "wrong identity".into())
            .is_err()
    );
    broker.reply(&id, "answer".into()).unwrap();
    assert!(broker.reply(&id, "stale".into()).is_err());
    assert_eq!(poll(waiting.as_mut()), Poll::Ready(Ok("answer".into())));
    assert!(broker.reject(&id).is_err());
    assert!(broker.withdraw(&id).is_err());
    let mut rejected = broker.ask(request(10, 2), &live);
    assert!(poll(rejected.as_mut()).is_pending());
    broker.reject(&request(10, 2).id).unwrap();
    assert_eq!(ready(rejected), Err(OperationError::Failed("rejected")));
    let mut withdrawn = broker.ask(request(10, 3), &live);
    assert!(poll(withdrawn.as_mut()).is_pending());
    broker.withdraw(&request(10, 3).id).unwrap();
    assert_eq!(ready(withdrawn), Err(OperationError::Failed("withdrawn")));
    assert!(matches!(
        ready(broker.ask(request(11, 3), &live)),
        Err(OperationError::Failed(_))
    ));
    let cancellation = Cancel::default();
    let mut cancelled = broker.ask(request(10, 4), &cancellation);
    assert!(poll(cancelled.as_mut()).is_pending());
    cancellation.cancel();
    assert_eq!(ready(cancelled), Err(OperationError::Cancelled));
    assert!(broker.pending().unwrap().is_empty());
    assert!(broker.reply(&request(10, 4).id, "too late".into()).is_err());
}

#[test]
fn permissions_never_fabricate_approval_on_cancellation_or_wrong_scope() {
    let broker = Permissions(Broker::new(10));
    let cancelled = Cancel::default();
    let id = request(10, 1).id;
    let mut waiting = broker.request(request(10, 1), &cancelled);
    assert!(poll(waiting.as_mut()).is_pending());
    cancelled.cancel();
    assert_eq!(
        poll(waiting.as_mut()),
        Poll::Ready(Err(OperationError::Cancelled))
    );
    assert!(ready(broker.pending()).unwrap().is_empty());
    assert!(ready(broker.reply(&id, "approved".into())).is_err());
    assert!(ready(broker.reject(&id)).is_err());
    assert!(ready(broker.withdraw(&id)).is_err());
    let live = Cancel::default();
    let mut rejected = broker.request(request(10, 2), &live);
    assert!(poll(rejected.as_mut()).is_pending());
    assert!(ready(broker.reply(&request(11, 2).id, "wrong scope".into())).is_err());
    ready(broker.reject(&request(10, 2).id)).unwrap();
    assert_eq!(ready(rejected), Err(OperationError::Failed("rejected")));
    let mut withdrawn = broker.request(request(10, 3), &live);
    assert!(poll(withdrawn.as_mut()).is_pending());
    ready(broker.withdraw(&request(10, 3).id)).unwrap();
    assert_eq!(ready(withdrawn), Err(OperationError::Failed("withdrawn")));
    let mut approved = broker.request(request(10, 4), &live);
    assert!(poll(approved.as_mut()).is_pending());
    ready(broker.reply(&request(10, 4).id, "approved".into())).unwrap();
    assert_eq!(ready(approved), Ok("approved".into()));
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
