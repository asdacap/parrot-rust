use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_cli::presenters::{
    EnhancedSessionPresenter, ImmediateScrollbackValue, SessionReplayContext, ToolPresenterRegistry,
};
use parrot_domain::protocol::messages::{parrot as wire, queue};
use protobuf::Message;
use wire::event::Payload;

use super::turn::{child, commits, completed, ended, event, live, start, text};

fn configured(columns: usize) -> EnhancedSessionPresenter {
    let mut presenter = EnhancedSessionPresenter::new(columns, false, false);
    presenter.set_session("build", "provider/model").unwrap();
    presenter
}
#[test]
fn resize_preserves_pending_live_text_and_zero_width_clamps_like_one() {
    let mut outputs = Vec::new();
    for columns in [8, 0, 1] {
        let mut presenter = configured(80);
        presenter.observe(&start("root")).unwrap();
        let pending = presenter.observe(&text("root", "abcdefghij")).unwrap();
        assert!(pending.commits.is_empty());
        assert!(live(&pending.live, 80).contains("● abcdefghij\n"));

        let resized = presenter.resize(columns).unwrap();
        assert!(resized.commits.is_empty());
        assert_eq!(resized.completion, None);
        assert!(!resized.finished_main_turn);
        assert!(!live(&resized.live, columns.max(1)).contains("abcdefghij"));
        let appended = presenter.observe(&text("root", "klmn")).unwrap();
        assert!(appended.commits.is_empty());
        assert!(live(&appended.live, 80).contains("● abcdefghijklmn\n"));
        let committed = presenter.observe(&completed("root")).unwrap();
        assert_eq!(committed.completion, Some(true));
        assert!(committed.finished_main_turn);
        let output = commits(&committed.commits, 80);
        assert_eq!(
            output
                .chars()
                .filter(char::is_ascii_alphabetic)
                .collect::<String>(),
            "abcdefghijklmn"
        );
        assert!(!output.contains("● abcdefghijklmn\r\n"));
        assert!(presenter.finish().unwrap().commits.is_empty());
        outputs.push(output);
    }
    assert_eq!(outputs[0], "● abcdef\r\n  ghijkl\r\n  mn\r\n");
    assert_eq!(outputs[1], outputs[2]);
}

fn phase(owner: &str, phase: wire::ProviderRequestPhase, attempt: u32) -> wire::Event {
    event(
        owner,
        Payload::ProviderRequestPhaseChanged(wire::ProviderRequestPhaseChangedEvent {
            phase: phase.into(),
            attempt,
            ..Default::default()
        }),
    )
}
fn request_phase_case(ending: &str) {
    use wire::ProviderRequestPhase::*;
    let mut presenter = configured(160);
    presenter.observe(&start("root")).unwrap();
    for (owner, phase_value, attempt, label) in [
        ("root", PROVIDER_REQUEST_PHASE_REQUESTING, 1, "Requesting…"),
        (
            "child",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            2,
            "Requesting…",
        ),
        (
            "root",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            1,
            "Waiting for first token…",
        ),
        (
            "child",
            PROVIDER_REQUEST_PHASE_IDLE,
            2,
            "Waiting for first token…",
        ),
        ("root", PROVIDER_REQUEST_PHASE_IDLE, 1, ""),
        (
            "root",
            PROVIDER_REQUEST_PHASE_REQUESTING,
            2,
            "Requesting (attempt 2)…",
        ),
        ("root", PROVIDER_REQUEST_PHASE_IDLE, 2, ""),
        ("root", PROVIDER_REQUEST_PHASE_REQUESTING, 1, "Requesting…"),
        (
            "root",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            1,
            "Waiting for first token…",
        ),
        ("root", PROVIDER_REQUEST_PHASE_UNSPECIFIED, 1, ""),
        (
            "root",
            PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED,
            1,
            "Waiting for first token…",
        ),
    ] {
        let update = presenter
            .observe(&phase(owner, phase_value, attempt))
            .unwrap();
        assert!(update.commits.is_empty());
        let frame = live(&update.live, 160);
        assert_eq!(
            frame.contains("Requesting"),
            label.starts_with("Requesting"),
            "{frame:?}"
        );
        assert_eq!(
            frame.contains("Waiting for first token…"),
            label == "Waiting for first token…"
        );
        if !label.is_empty() {
            assert!(frame.contains(&format!("{label} (running")));
        }
        assert!(frame.contains("provider/model"));
        assert!(frame.contains("mode: build"));
    }
    let child_ended = presenter.observe(&ended("child")).unwrap();
    assert_eq!(child_ended.completion, None);
    assert!(!child_ended.finished_main_turn);
    assert!(live(&child_ended.live, 160).contains("Waiting for first token… (running"));
    if ending == "ended" || ending == "failed" {
        let terminal = if ending == "ended" {
            ended("root")
        } else {
            event(
                "root",
                Payload::TurnFailed(wire::TurnFailed {
                    message: "failed".into(),
                    ..Default::default()
                }),
            )
        };
        let update = presenter.observe(&terminal).unwrap();
        if ending == "ended" {
            assert_eq!(update.completion, None);
            assert!(!update.finished_main_turn);
        }
        let frame = live(&update.live, 160);
        assert!(!frame.contains("Requesting…"));
        assert!(!frame.contains("Waiting for first token…"));
    }
    let update = if ending == "cancel" {
        presenter.cancel().unwrap()
    } else {
        presenter.finish().unwrap()
    };
    let frame = live(&update.live, 160);
    assert!(!frame.contains("Requesting…"));
    assert!(!frame.contains("Waiting for first token…"));
}
macro_rules! request_cases {($($name:ident:$value:literal),*$(,)?)=>{$(#[test] fn $name(){request_phase_case($value);})*}}
request_cases! { request_phase_eof:"eof", request_phase_cancel:"cancel", request_phase_ended:"ended", request_phase_failed:"failed" }

#[test]
fn live_usage_is_modeline_only_aggregates_all_agents_expires_and_resets() {
    let mut presenter = configured(120);
    let update = presenter
        .observe(&event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot {
                revision: 1,
                input_tokens: 100,
                cached_input_tokens: 25,
                output_tokens: 20,
                context_size: 50,
                context_limit: 1000,
                input_cost: 0.5,
                output_cost: 0.25,
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(update.commits.is_empty());
    for owner in ["root", "child"] {
        let update = presenter
            .observe(&event(
                owner,
                Payload::ProviderCallUsage(wire::ProviderCallUsage {
                    input_tokens: 30,
                    output_tokens: 30,
                    ..Default::default()
                }),
            ))
            .unwrap();
        assert!(!update.live_event);
        assert!(update.commits.is_empty());
    }
    for published in [start("root"), ended("root"), completed("root")] {
        presenter.observe(&published).unwrap();
    }
    let frame = live(&presenter.snapshot().unwrap(), 120);
    for value in ["+100i +20o (+25.00% cache)", "50/1k", "2i/s 2o/s", "$0.75"] {
        assert!(frame.contains(value), "missing {value} in {frame:?}");
    }
    for hidden in [
        "Working:",
        "Thinking…",
        "ProviderCallUsage",
        "InputTokens",
        "OutputTokens",
    ] {
        assert!(!frame.contains(hidden));
    }
    let committed = presenter
        .commit(Arc::new(ImmediateScrollbackValue::trusted(vec![
            "committed sentinel".into(),
        ])))
        .unwrap();
    assert_eq!(committed.commits.len(), 1);
    let output = commits(&committed.commits, 120);
    assert_eq!(output.matches("committed sentinel").count(), 1);
    for hidden in ["ProviderCallUsage", "InputTokens", "OutputTokens"] {
        assert!(!output.contains(hidden));
    }
    let expired = presenter.tick(Duration::from_secs(30)).unwrap();
    assert!(!live(&expired.live, 120).contains("i/s"));
    let replacement = configured(120);
    let reset = live(&replacement.snapshot().unwrap(), 120);
    assert!(!reset.contains("i/s"));
    assert!(!reset.contains("+100i"));
}

fn retry_case(maximum_retries: i32) {
    let mut presenter = configured(140);
    presenter.observe(&start("root")).unwrap();
    let mut output = String::new();
    for attempt in 1..=maximum_retries {
        let update = presenter
            .observe(&event(
                "root",
                Payload::RetryNotice(wire::RetryNotice {
                    attempt,
                    retry_after_ms: 2000,
                    reason: "Provider response headers timed out. Retrying.".into(),
                    ..Default::default()
                }),
            ))
            .unwrap();
        output.push_str(&commits(&update.commits, 140));
    }
    let message = format!(
        "Provider header timeout retry limit exceeded ({maximum_retries} retries, {} attempts).",
        maximum_retries + 1
    );
    let failed = presenter
        .observe(&event(
            "root",
            Payload::TurnFailed(wire::TurnFailed {
                message: message.clone(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(failed.completion, Some(false));
    assert!(failed.finished_main_turn);
    assert!((output + &failed.errors.join("\n")).contains(&message));
    let frame = live(&failed.live, 140);
    assert!(!frame.contains("running"));
    assert!(!frame.contains("Retrying."));
}
macro_rules! retry_cases {($($name:ident:$value:literal),*$(,)?)=>{$(#[test] fn $name(){retry_case($value);})*}}
retry_cases! { retry_exhaustion_zero:0, retry_exhaustion_one:1, retry_exhaustion_five:5 }

#[test]
fn root_turn_duration_advances_and_respects_lifecycle_boundaries() {
    let mut presenter = configured(120);
    let first = presenter.observe(&start("root")).unwrap();
    assert!(live(&first.live, 120).contains("agent main (running 0s)"));
    let elapsed = presenter.tick(Duration::from_secs(65)).unwrap();
    assert!(live(&elapsed.live, 120).contains("agent main (running 1m 05s)"));
    let working = presenter
        .observe(&event(
            "root",
            Payload::ToolStarted(wire::ToolStarted {
                tool_call_id: "wait-call".into(),
                tool_name: "wait".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(live(&working.live, 120).contains("Working: wait (running 1m 05s)"));
    let elapsed = presenter.tick(Duration::from_secs(7384)).unwrap();
    assert!(live(&elapsed.live, 120).contains("Working: wait (running 2h 03m 04s)"));
    for published in [child("root"), start("child"), ended("child")] {
        let update = presenter.observe(&published).unwrap();
        assert_eq!(update.completion, None);
        assert!(!update.finished_main_turn);
        assert!(live(&update.live, 120).contains("Working: wait (running 2h 03m 04s)"));
    }
    assert!(
        live(&presenter.snapshot().unwrap(), 120).contains("Working: wait (running 2h 03m 04s)")
    );
    let done = presenter.observe(&ended("root")).unwrap();
    assert_eq!(done.completion, None);
    assert!(!done.finished_main_turn);
    assert!(!live(&done.live, 120).contains("running"));
    let again = presenter.observe(&start("root")).unwrap();
    assert!(live(&again.live, 120).contains("agent main (running 0s)"));
    let failed = presenter
        .observe(&event(
            "root",
            Payload::TurnFailed(wire::TurnFailed {
                message: "failed".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert!(!live(&failed.live, 120).contains("running"));
    presenter.observe(&start("root")).unwrap();
    let later = presenter.tick(Duration::from_secs(7396)).unwrap();
    assert!(live(&later.live, 120).contains("agent main (running 12s)"));
    let replacement = configured(120);
    assert!(!live(&replacement.snapshot().unwrap(), 120).contains("running"));
}

#[test]
fn older_stream_without_live_usage_keeps_rates_out_of_modeline() {
    let mut presenter = configured(120);
    presenter
        .observe(&event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot {
                revision: 1,
                input_tokens: 100,
                output_tokens: 20,
                context_size: 50,
                context_limit: 1000,
                ..Default::default()
            }),
        ))
        .unwrap();
    for published in [start("root"), ended("root"), completed("root")] {
        presenter.observe(&published).unwrap();
    }
    let frame = live(&presenter.snapshot().unwrap(), 120);
    assert!(frame.contains("+100i +20o"));
    assert!(frame.contains("50/1k"));
    assert!(!frame.contains("i/s"));
    assert!(!frame.contains("o/s"));
}

pub(crate) fn queue_event(owner: &str, queues: Vec<queue::QueueState>) -> wire::Event {
    let snapshot = queue::QueueSnapshot {
        root_agent_session_id: "root".into(),
        queues,
        ..Default::default()
    };
    event(
        "",
        Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "queue".into(),
            owner_agent_session_id: owner.into(),
            inventory_instance_id: "inventory".into(),
            revision: 1,
            final_chunk: true,
            data: snapshot.write_to_bytes().unwrap(),
            ..Default::default()
        }),
    )
}

fn boundary_case(kind: &str, owner: &str, replaying: bool) {
    // This is the source's constant RunWithReplay predicate, not prepared-stream state.
    let context = Arc::new(Mutex::new(SessionReplayContext::new(replaying)));
    let mut presenter = EnhancedSessionPresenter::with_registry(
        120,
        false,
        context,
        Arc::new(ToolPresenterRegistry::standard(Vec::new())),
    );
    presenter.set_session("build", "provider/model").unwrap();
    let boundary = match kind {
        "queue" => queue_event(owner, Vec::new()),
        "reasoning" => event(
            owner,
            Payload::EncryptedReasoningReceived(wire::EncryptedReasoningReceivedEvent::default()),
        ),
        _ => event(
            owner,
            Payload::AgentFinished(wire::AgentFinished::default()),
        ),
    };
    let events = [
        start("root"),
        child("root"),
        start("child"),
        text("root", "root before"),
        text("child", "child before"),
        event(
            owner,
            Payload::ProviderCallUsage(wire::ProviderCallUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            }),
        ),
        event(
            "",
            Payload::SessionUsageSnapshot(wire::SessionUsageSnapshot {
                revision: 1,
                ..Default::default()
            }),
        ),
        boundary,
        text("root", "root after"),
        text("child", "child after"),
        ended("child"),
        ended("root"),
        completed("root"),
    ];
    let mut output = String::new();
    let mut main_turns = 0;
    for published in events {
        let update = presenter.observe(&published).unwrap();
        output.push_str(&commits(&update.commits, 120));
        main_turns += usize::from(update.started_main_turn);
        assert!(update.errors.is_empty());
        if matches!(
            published.payload,
            Some(
                Payload::ProviderCallUsage(_)
                    | Payload::ToolSnapshot(_)
                    | Payload::AgentFinished(_)
            )
        ) {
            assert!(!update.live_event);
        }
    }
    let prefix = if owner == "root" {
        "● "
    } else {
        "  ● [worker] "
    };
    assert!(
        output.contains(&format!("{prefix}{owner} after\r\n")),
        "{output:?}"
    );
    assert!(
        !output.contains(&format!("{owner} before{owner} after")),
        "{output:?}"
    );
    let other = if owner == "root" { "child" } else { "root" };
    assert!(
        output.contains(&format!("{other} before{other} after")),
        "{output:?}"
    );
    assert_eq!(main_turns, usize::from(!replaying));
}
macro_rules! boundaries {($($name:ident:($kind:literal,$owner:literal,$replaying:literal)),*$(,)?)=>{$(#[test]fn $name(){boundary_case($kind,$owner,$replaying);})*}}
boundaries! {
    queue_root_live:("queue","root",false), queue_child_live:("queue","child",false), queue_root_replay:("queue","root",true), queue_child_replay:("queue","child",true),
    reasoning_root_live:("reasoning","root",false), reasoning_child_live:("reasoning","child",false), reasoning_root_replay:("reasoning","root",true), reasoning_child_replay:("reasoning","child",true),
    lifecycle_root_live:("lifecycle","root",false), lifecycle_root_replay:("lifecycle","root",true),
}

#[test]
fn orders_and_deduplicates_queue_snapshot_and_starts_only_main_turn() {
    let mut presenter = configured(100);
    let queues = [
        ("zeta", "last queue", 1),
        ("alpha", "stale queue", 2),
        ("alpha", "first queue", 3),
        ("empty", "hidden queue", 0),
    ]
    .into_iter()
    .map(|(name, description, item_count)| queue::QueueState {
        owner_agent_session_id: "root".into(),
        name: name.into(),
        description: description.into(),
        item_count,
        ..Default::default()
    })
    .collect();
    presenter.observe(&queue_event("root", queues)).unwrap();
    let mut main_turns = 0;
    let mut completion = None;
    for published in [
        start("root"),
        child("root"),
        start("child"),
        ended("root"),
        completed("root"),
    ] {
        let update = presenter.observe(&published).unwrap();
        main_turns += usize::from(update.started_main_turn);
        completion = update.completion.or(completion);
    }
    let frame = live(&presenter.snapshot().unwrap(), 100);
    let alpha = frame
        .find("queue: alpha · 3 items — first queue")
        .expect(&frame);
    let zeta = frame
        .find("queue: zeta · 1 item — last queue")
        .expect(&frame);
    assert_eq!(completion, Some(true));
    assert_eq!(main_turns, 1);
    assert!(zeta > alpha);
    assert!(!frame.contains("stale queue"));
    assert!(!frame.contains("hidden queue"));
}
