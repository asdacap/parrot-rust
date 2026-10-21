use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, Cancelled, Clock};
use parrot_domain::values::activity::*;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

struct ControlledClock {
    now: Mutex<Instant>,
}

impl ControlledClock {
    fn new() -> Self {
        Self {
            now: Mutex::new(Instant::now()),
        }
    }

    fn advance(&self, seconds: i64) {
        let mut now = self.now.lock().unwrap();
        let duration = Duration::from_secs(seconds.unsigned_abs());
        *now = if seconds < 0 {
            *now - duration
        } else {
            *now + duration
        };
    }
}

impl Clock for ControlledClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }

    fn utc_now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    fn sleep_until<'a>(
        &'a self,
        _: Instant,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async { panic!("activity must not sleep") })
    }
}

fn setup() -> (Arc<ControlledClock>, AgentSessionActivity) {
    let clock = Arc::new(ControlledClock::new());
    let activity = AgentSessionActivity::new(clock.clone());
    (clock, activity)
}

fn summary(text: &str, part: &str, completed: bool) -> LlmEvent {
    LlmEvent::reasoning_delta(text, LlmReasoningKind::Summary, part, completed)
}

#[test]
fn provider_activity_excludes_completed_and_raw_reasoning_content() {
    let (time, activity) = setup();
    activity.observe_provider_event(&LlmEvent::new(LlmEventKind::Completed));
    assert_eq!(activity.capture().latest_provider_activity_age, None);
    activity.observe_provider_event(&LlmEvent::reasoning_delta(
        "private",
        LlmReasoningKind::Raw,
        "",
        true,
    ));
    let empty = activity.capture();
    activity.observe_provider_event(&LlmEvent::text_delta("text"));
    time.advance(1);
    activity.observe_provider_event(&LlmEvent::reasoning_delta(
        "reasoning",
        LlmReasoningKind::Raw,
        "",
        false,
    ));
    time.advance(2);
    activity.observe_provider_event(&LlmEvent::new(LlmEventKind::ToolCallDelta));
    time.advance(3);
    activity.observe_provider_event(&LlmEvent::new(LlmEventKind::Retry));
    time.advance(4);
    activity.observe_provider_event(&LlmEvent::new(LlmEventKind::Completed));
    let captured = activity.capture();
    assert_eq!(empty.latest_provider_activity_age, Some(Duration::ZERO));
    assert!(empty.recent.is_empty());
    assert_eq!(
        captured.latest_provider_activity_age,
        Some(Duration::from_secs(4))
    );
    assert!(captured.recent.is_empty());
}

#[test]
fn named_and_sequential_unnamed_summaries_close_atomically() {
    let (_, activity) = setup();
    activity.observe_provider_event(&summary("named ", "part", false));
    activity.observe_provider_event(&summary("summary", "part", false));
    assert!(activity.capture().recent.is_empty());
    activity.observe_provider_event(&summary("", "part", true));
    activity.observe_provider_event(&summary("first", "", false));
    activity.observe_provider_event(&summary("", "", true));
    activity.observe_provider_event(&summary("second", "", true));
    let captured = activity.capture();
    assert_eq!(
        captured
            .recent
            .iter()
            .map(|entry| entry.content.as_str())
            .collect::<Vec<_>>(),
        ["named summary", "first", "second"]
    );
    assert!(
        captured
            .recent
            .iter()
            .all(|entry| entry.kind == AgentSessionActivityEntryKind::ReasoningSummary)
    );
}

#[test]
fn request_cleanup_discards_unfinished_summaries_and_blank_completion() {
    let (_, activity) = setup();
    activity.observe_provider_event(&summary("stale named", "reused", false));
    activity.observe_provider_event(&summary("stale unnamed", "", false));
    activity.finish_provider_request();
    activity.observe_provider_event(&summary("fresh named", "reused", true));
    activity.observe_provider_event(&summary("fresh unnamed", "", true));
    activity.observe_provider_event(&summary("   ", "blank", true));
    let captured = activity.capture();
    assert_eq!(
        captured
            .recent
            .iter()
            .map(|entry| entry.content.as_str())
            .collect::<Vec<_>>(),
        ["fresh named", "fresh unnamed"]
    );
}

#[test]
fn request_session_duration_advances_while_active_and_stops_at_completion() {
    let (time, activity) = setup();
    assert_eq!(activity.capture().request_session_duration, None);
    let execution = activity.begin_execution();
    time.advance(1);
    assert_eq!(
        activity.capture().request_session_duration,
        Some(Duration::from_secs(1))
    );
    activity.finish_execution(execution, AgentExecution::succeeded(""));
    time.advance(2);
    assert_eq!(
        activity.capture().request_session_duration,
        Some(Duration::from_secs(1))
    );
}

#[test]
fn older_execution_completion_does_not_finish_newer_request_session() {
    let (time, activity) = setup();
    let older = activity.begin_execution();
    time.advance(1);
    let current = activity.begin_execution();
    time.advance(2);
    activity.finish_execution(older, AgentExecution::succeeded(""));
    let active = activity.capture();
    assert_eq!(
        active.request_session_duration,
        Some(Duration::from_secs(2))
    );
    assert_eq!(active.terminal_outcome, None);
    activity.finish_execution(current, AgentExecution::canceled());
    assert_eq!(
        activity.capture().terminal_outcome.unwrap().status,
        AgentExecutionStatus::Canceled
    );
}

#[test]
fn provider_request_durations_distinguish_current_and_last_completed_requests() {
    let (time, activity) = setup();
    activity.begin_provider_request();
    time.advance(1);
    let active = activity.capture();
    assert_eq!(
        active.current_provider_request_duration,
        Some(Duration::from_secs(1))
    );
    assert_eq!(active.last_provider_request_duration, None);
    activity.finish_provider_request();
    time.advance(2);
    let completed = activity.capture();
    assert_eq!(completed.current_provider_request_duration, None);
    assert_eq!(
        completed.last_provider_request_duration,
        Some(Duration::from_secs(1))
    );
    activity.begin_provider_request();
    time.advance(3);
    let next = activity.capture();
    assert_eq!(
        next.current_provider_request_duration,
        Some(Duration::from_secs(3))
    );
    assert_eq!(
        next.last_provider_request_duration,
        Some(Duration::from_secs(1))
    );
}

#[test]
fn assistant_and_summary_entries_share_oldest_to_newest_five_entry_bound() {
    let (_, activity) = setup();
    activity.record_assistant_message("dropped");
    activity.observe_provider_event(&summary("summary-1", "one", true));
    activity.record_assistant_message("assistant-2");
    activity.observe_provider_event(&summary("summary-3", "three", true));
    activity.record_assistant_message("assistant-4");
    activity.record_assistant_message("assistant-5");
    let captured = activity.capture();
    assert_eq!(
        captured
            .recent
            .iter()
            .map(|entry| entry.content.as_str())
            .collect::<Vec<_>>(),
        [
            "summary-1",
            "assistant-2",
            "summary-3",
            "assistant-4",
            "assistant-5"
        ]
    );
    assert_eq!(
        captured.recent[0].kind,
        AgentSessionActivityEntryKind::ReasoningSummary
    );
    assert_eq!(
        captured.recent[1].kind,
        AgentSessionActivityEntryKind::AssistantMessage
    );
    assert_eq!(
        captured
            .recent
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [2, 3, 4, 5, 6]
    );
}

#[test]
fn snapshot_collection_is_a_copy_without_array_mutability() {
    let (_, activity) = setup();
    activity.record_assistant_message("first");
    let captured = activity.capture();
    activity.record_assistant_message("second");
    assert_eq!(captured.recent.len(), 1);
    assert_eq!(captured.recent[0].content, "first");
}

#[test]
fn concurrent_capture_returns_coherent_bounded_snapshots() {
    let (_, activity) = setup();
    thread::scope(|scope| {
        scope.spawn(|| {
            for index in 0..1000 {
                activity.record_assistant_message(&format!("entry-{index}"));
            }
        });
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..1000 {
                    let captured = activity.capture();
                    assert!(captured.recent.len() <= 5);
                    assert!(
                        captured
                            .recent
                            .iter()
                            .all(|entry| !entry.content.trim().is_empty())
                    );
                }
            });
        }
    });
    assert_eq!(activity.capture().recent.len(), 5);
}

#[test]
fn tool_and_lifecycle_state_clear_and_retain_terminal_outcome() {
    let (_, activity) = setup();
    activity.change_state(DrainState::Running);
    let older = activity.begin_tool("first");
    let current = activity.begin_tool("second");
    activity.finish_tool(older);
    assert_eq!(activity.capture().current_tool.as_deref(), Some("second"));
    activity.finish_tool(current);
    activity.change_state(DrainState::Interrupting);
    activity.change_state(DrainState::Idle);
    let execution = activity.begin_execution();
    let outcome = AgentExecution::canceled();
    activity.finish_execution(execution, outcome.clone());
    let terminal = activity.capture();
    assert_eq!(terminal.current_tool, None);
    assert_eq!(terminal.state, DrainState::Idle);
    assert_eq!(terminal.terminal_outcome, Some(outcome));
    activity.begin_execution();
    assert_eq!(activity.capture().terminal_outcome, None);
}

#[test]
fn elapsed_ages_use_monotonic_time_and_clamp_negative_values() {
    let (time, activity) = setup();
    activity.observe_provider_event(&LlmEvent::text_delta("text"));
    activity.record_assistant_message("answer");
    time.advance(5);
    let advanced = activity.capture();
    time.advance(-10);
    let rewound = activity.capture();
    assert_eq!(
        advanced.latest_provider_activity_age,
        Some(Duration::from_secs(5))
    );
    assert_eq!(advanced.recent[0].age, Duration::from_secs(5));
    assert_eq!(rewound.latest_provider_activity_age, Some(Duration::ZERO));
    assert_eq!(rewound.recent[0].age, Duration::ZERO);
}
