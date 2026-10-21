//! Bridges the accepted domain activity tracker, without duplicating its timing state.
use super::{lifecycle::Execution, provider_call::ProviderActivity, services::SessionActivity};
use parrot_domain::values::activity::{self as domain, AgentSessionActivity};
use parrot_provider::sessions;
use std::{sync::Arc, time::Duration};

pub trait TurnDiagnostics: Send + Sync {
    fn started(&self, turn_id: &str);
    fn finished(&self, turn_id: &str, outcome: &str, error_code: Option<&str>);
}

pub struct LiveActivity {
    tracker: Arc<AgentSessionActivity>,
    diagnostics: Arc<dyn TurnDiagnostics>,
}
impl LiveActivity {
    pub fn new(tracker: Arc<AgentSessionActivity>, diagnostics: Arc<dyn TurnDiagnostics>) -> Self {
        Self {
            tracker,
            diagnostics,
        }
    }
    pub fn tracker(&self) -> &Arc<AgentSessionActivity> {
        &self.tracker
    }
}
impl ProviderActivity for LiveActivity {
    fn begin_provider_request(&self) {
        self.tracker.begin_provider_request();
    }
    fn finish_provider_request(&self) {
        self.tracker.finish_provider_request();
    }
    fn observe_provider_event(&self, event: &sessions::Event) {
        let kind = match event.base_event() {
            sessions::Event::TextDelta(_) => domain::LlmEventKind::TextDelta,
            sessions::Event::ReasoningDelta { .. } => domain::LlmEventKind::ReasoningDelta,
            sessions::Event::ToolCallDelta(_) => domain::LlmEventKind::ToolCallDelta,
            sessions::Event::Retry { .. } => domain::LlmEventKind::Retry,
            sessions::Event::Completed(_) => domain::LlmEventKind::Completed,
            sessions::Event::HttpRequestStarted => domain::LlmEventKind::HttpRequestStarted,
            sessions::Event::HttpResponseHeadersReceived => {
                domain::LlmEventKind::HttpResponseHeadersReceived
            }
            sessions::Event::EncryptedReasoningReceived => {
                domain::LlmEventKind::EncryptedReasoningReceived
            }
            sessions::Event::WithText { .. } => unreachable!("base_event unwraps text envelope"),
        };
        let mut observed = domain::LlmEvent::new(kind);
        observed.text = event.text().to_owned();
        if let sessions::Event::ReasoningDelta {
            kind,
            part_id,
            completed,
            ..
        } = event.base_event()
        {
            observed.reasoning_kind = match kind {
                sessions::ReasoningKind::Raw => domain::LlmReasoningKind::Raw,
                sessions::ReasoningKind::Summary => domain::LlmReasoningKind::Summary,
            };
            observed.reasoning_part_id = part_id.clone();
            observed.reasoning_completed = *completed;
        }
        self.tracker.observe_provider_event(&observed);
    }
}
impl SessionActivity for LiveActivity {
    fn change_state(&self, state: super::lifecycle::DrainState) {
        self.tracker.change_state(state);
    }
    fn begin_execution(&self) -> i64 {
        self.tracker.begin_execution()
    }
    fn execution_duration(&self) -> Duration {
        self.tracker
            .capture()
            .request_session_duration
            .unwrap_or_default()
    }
    fn finish_execution(&self, execution: i64, outcome: &Execution) {
        self.tracker.finish_execution(execution, outcome.clone());
    }
    fn turn_started(&self, turn_id: &str) {
        self.diagnostics.started(turn_id);
    }
    fn turn_finished(&self, turn_id: &str, outcome: &str, error_code: Option<&str>) {
        self.diagnostics.finished(turn_id, outcome, error_code);
    }
    fn assistant_message(&self, message: &str) {
        self.tracker.record_assistant_message(message);
    }
}
