use crate::contracts::async_ops::Clock;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DrainState {
    #[default]
    Idle,
    Running,
    Interrupting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentExecutionStatus {
    Succeeded,
    Failed,
    Canceled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentExecution {
    pub status: AgentExecutionStatus,
    pub output: String,
    pub error: String,
}

impl AgentExecution {
    pub fn succeeded(output: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Succeeded,
            output: output.into(),
            error: String::new(),
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: AgentExecutionStatus::Failed,
            output: String::new(),
            error: error.into(),
        }
    }

    pub fn canceled() -> Self {
        Self {
            status: AgentExecutionStatus::Canceled,
            output: String::new(),
            error: "interrupted".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlmEventKind {
    TextDelta,
    ReasoningDelta,
    ToolCallDelta,
    Retry,
    Completed,
    HttpRequestStarted,
    HttpResponseHeadersReceived,
    EncryptedReasoningReceived,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LlmReasoningKind {
    #[default]
    Raw,
    Summary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlmEvent {
    pub kind: LlmEventKind,
    pub text: String,
    pub reasoning_kind: LlmReasoningKind,
    pub reasoning_part_id: String,
    pub reasoning_completed: bool,
}

impl LlmEvent {
    pub fn new(kind: LlmEventKind) -> Self {
        Self {
            kind,
            text: String::new(),
            reasoning_kind: LlmReasoningKind::Raw,
            reasoning_part_id: String::new(),
            reasoning_completed: false,
        }
    }

    pub fn text_delta(fragment: impl Into<String>) -> Self {
        Self {
            text: fragment.into(),
            ..Self::new(LlmEventKind::TextDelta)
        }
    }

    pub fn reasoning_delta(
        fragment: impl Into<String>,
        reasoning_kind: LlmReasoningKind,
        part_id: impl Into<String>,
        completed: bool,
    ) -> Self {
        Self {
            kind: LlmEventKind::ReasoningDelta,
            text: fragment.into(),
            reasoning_kind,
            reasoning_part_id: part_id.into(),
            reasoning_completed: completed,
        }
    }

    pub fn is_visible_output(&self) -> bool {
        matches!(
            self.kind,
            LlmEventKind::TextDelta
                | LlmEventKind::ReasoningDelta
                | LlmEventKind::ToolCallDelta
                | LlmEventKind::Completed
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSessionActivityEntryKind {
    ReasoningSummary,
    AssistantMessage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSessionActivityEntrySnapshot {
    pub kind: AgentSessionActivityEntryKind,
    pub content: String,
    pub age: Duration,
    pub sequence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSessionActivitySnapshot {
    pub state: DrainState,
    pub current_tool: Option<String>,
    pub request_session_duration: Option<Duration>,
    pub current_provider_request_duration: Option<Duration>,
    pub last_provider_request_duration: Option<Duration>,
    pub latest_provider_activity_age: Option<Duration>,
    pub terminal_outcome: Option<AgentExecution>,
    pub recent: Box<[AgentSessionActivityEntrySnapshot]>,
}

struct RecentEntry {
    kind: AgentSessionActivityEntryKind,
    content: String,
    timestamp: Instant,
    sequence: i64,
}

#[derive(Default)]
struct ActivityState {
    active_tools: BTreeMap<i64, String>,
    summaries: HashMap<String, String>,
    recent: VecDeque<RecentEntry>,
    current_provider_request_started: Option<Instant>,
    last_provider_request_duration: Option<Duration>,
    latest_provider_activity: Option<Instant>,
    request_session_started: Option<Instant>,
    request_session_duration: Option<Duration>,
    recent_sequence: i64,
    request_session_execution: i64,
    tool_execution: i64,
    state: DrainState,
    terminal_outcome: Option<AgentExecution>,
}

impl ActivityState {
    fn append_recent(
        &mut self,
        kind: AgentSessionActivityEntryKind,
        content: String,
        timestamp: Instant,
    ) {
        self.recent_sequence = self.recent_sequence.wrapping_add(1);
        self.recent.push_back(RecentEntry {
            kind,
            content,
            timestamp,
            sequence: self.recent_sequence,
        });
        if self.recent.len() > 5 {
            self.recent.pop_front();
        }
    }
}

pub struct AgentSessionActivity {
    clock: Arc<dyn Clock>,
    state: Mutex<ActivityState>,
}

impl AgentSessionActivity {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            state: Mutex::new(ActivityState::default()),
        }
    }

    pub fn observe_provider_event(&self, observed: &LlmEvent) {
        if observed.kind == LlmEventKind::Completed {
            return;
        }
        let mut state = self.state.lock().expect("activity lock poisoned");
        let timestamp = self.clock.now();
        state.latest_provider_activity = Some(timestamp);
        if observed.kind == LlmEventKind::ReasoningDelta
            && observed.reasoning_kind == LlmReasoningKind::Summary
        {
            state
                .summaries
                .entry(observed.reasoning_part_id.clone())
                .or_default()
                .push_str(&observed.text);
            if observed.reasoning_completed {
                let summary = state
                    .summaries
                    .remove(&observed.reasoning_part_id)
                    .expect("summary was accumulated");
                if !summary.trim().is_empty() {
                    state.append_recent(
                        AgentSessionActivityEntryKind::ReasoningSummary,
                        summary,
                        timestamp,
                    );
                }
            }
        }
    }

    pub fn begin_provider_request(&self) {
        let mut state = self.state.lock().expect("activity lock poisoned");
        state.current_provider_request_started = Some(self.clock.now());
    }

    pub fn finish_provider_request(&self) {
        let mut state = self.state.lock().expect("activity lock poisoned");
        if let Some(started) = state.current_provider_request_started.take() {
            state.last_provider_request_duration =
                Some(self.clock.now().saturating_duration_since(started));
        }
        state.summaries.clear();
    }

    pub fn record_assistant_message(&self, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("activity lock poisoned");
        state.append_recent(
            AgentSessionActivityEntryKind::AssistantMessage,
            content.to_owned(),
            self.clock.now(),
        );
    }

    pub fn begin_tool(&self, name: &str) -> i64 {
        assert!(!name.trim().is_empty(), "tool name must not be blank");
        let mut state = self.state.lock().expect("activity lock poisoned");
        state.tool_execution = state.tool_execution.wrapping_add(1);
        let execution = state.tool_execution;
        state.active_tools.insert(execution, name.to_owned());
        execution
    }

    pub fn finish_tool(&self, execution: i64) {
        self.state
            .lock()
            .expect("activity lock poisoned")
            .active_tools
            .remove(&execution);
    }

    pub fn change_state(&self, drain_state: DrainState) {
        self.state.lock().expect("activity lock poisoned").state = drain_state;
    }

    pub fn begin_execution(&self) -> i64 {
        let mut state = self.state.lock().expect("activity lock poisoned");
        state.request_session_execution = state.request_session_execution.wrapping_add(1);
        state.request_session_started = Some(self.clock.now());
        state.request_session_duration = None;
        state.terminal_outcome = None;
        state.request_session_execution
    }

    pub fn finish_execution(&self, execution: i64, outcome: AgentExecution) {
        let mut state = self.state.lock().expect("activity lock poisoned");
        if state.request_session_execution != execution {
            return;
        }
        if let Some(started) = state.request_session_started.take() {
            state.request_session_duration =
                Some(self.clock.now().saturating_duration_since(started));
        }
        state.terminal_outcome = Some(outcome);
    }

    pub fn capture(&self) -> AgentSessionActivitySnapshot {
        let state = self.state.lock().expect("activity lock poisoned");
        let timestamp = self.clock.now();
        let elapsed = |started| timestamp.saturating_duration_since(started);
        AgentSessionActivitySnapshot {
            state: state.state,
            current_tool: state
                .active_tools
                .last_key_value()
                .map(|(_, name)| name.clone()),
            request_session_duration: state
                .request_session_started
                .map(elapsed)
                .or(state.request_session_duration),
            current_provider_request_duration: state.current_provider_request_started.map(elapsed),
            last_provider_request_duration: state.last_provider_request_duration,
            latest_provider_activity_age: state.latest_provider_activity.map(elapsed),
            terminal_outcome: state.terminal_outcome.clone(),
            recent: state
                .recent
                .iter()
                .map(|entry| AgentSessionActivityEntrySnapshot {
                    kind: entry.kind,
                    content: entry.content.clone(),
                    age: elapsed(entry.timestamp),
                    sequence: entry.sequence,
                })
                .collect(),
        }
    }
}
