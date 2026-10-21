use super::*;
use parrot_domain::{
    contracts::async_ops::{Cancelled, Clock},
    values::activity::AgentSessionActivity,
};
use parrot_runtime::agent_turn::{
    activity::{LiveActivity, TurnDiagnostics},
    lifecycle::DrainState,
};
use std::time::{Instant, SystemTime};

pub(super) struct ManualClock {
    base: Instant,
    elapsed: AtomicU64,
}
impl ManualClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: AtomicU64::new(0),
        }
    }
    pub(super) fn advance(&self, millis: u64) {
        self.elapsed.fetch_add(millis, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.elapsed.load(Ordering::SeqCst))
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(self.elapsed.load(Ordering::SeqCst))
    }
    fn sleep_until<'a>(
        &'a self,
        _: Instant,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        panic!("activity tracker does not sleep")
    }
}
struct Diagnostics(Arc<Mutex<Vec<String>>>);
impl TurnDiagnostics for Diagnostics {
    fn started(&self, id: &str) {
        self.0.lock().unwrap().push(format!("started:{id}"));
    }
    fn finished(&self, id: &str, outcome: &str, code: Option<&str>) {
        self.0
            .lock()
            .unwrap()
            .push(format!("finished:{id}:{outcome}:{code:?}"));
    }
}
pub(super) struct Activity {
    pub(super) diagnostic: Arc<Mutex<Vec<String>>>,
    pub(super) live: LiveActivity,
    pub(super) clock: Arc<ManualClock>,
    trace: Arc<Trace>,
}
impl Activity {
    pub(super) fn new(trace: Arc<Trace>) -> Self {
        let clock = Arc::new(ManualClock::new());
        let diagnostic = Arc::new(Mutex::new(vec![]));
        let tracker = Arc::new(AgentSessionActivity::new(clock.clone()));
        Self {
            diagnostic: diagnostic.clone(),
            live: LiveActivity::new(tracker, Arc::new(Diagnostics(diagnostic))),
            clock,
            trace,
        }
    }
}
impl ProviderActivity for Activity {
    fn begin_provider_request(&self) {
        self.live.begin_provider_request();
    }
    fn observe_provider_event(&self, event: &sessions::Event) {
        self.live.observe_provider_event(event);
    }
    fn finish_provider_request(&self) {
        self.live.finish_provider_request();
    }
}
impl SessionActivity for Activity {
    fn change_state(&self, state: DrainState) {
        self.live.change_state(state);
    }
    fn begin_execution(&self) -> i64 {
        self.trace.push("execution-began");
        self.live.begin_execution()
    }
    fn execution_duration(&self) -> Duration {
        self.live.execution_duration()
    }
    fn finish_execution(&self, id: i64, execution: &Execution) {
        self.live.finish_execution(id, execution);
        self.trace
            .push(format!("execution-finished:{:?}", execution.status));
    }
    fn turn_started(&self, id: &str) {
        self.live.turn_started(id);
    }
    fn turn_finished(&self, id: &str, outcome: &str, error: Option<&str>) {
        self.live.turn_finished(id, outcome, error);
    }
    fn assistant_message(&self, text: &str) {
        self.live.assistant_message(text);
    }
}
