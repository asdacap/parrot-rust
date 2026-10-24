use std::time::Duration;

use parrot_domain::values::activity::{
    AgentExecution, AgentSessionActivityEntryKind, AgentSessionActivityEntrySnapshot,
    AgentSessionActivitySnapshot, DrainState,
};
use parrot_tools::agents::status::{StatusChild, StatusFormatter, StatusProcess, StatusSnapshot};

pub(super) fn snapshot() -> StatusSnapshot {
    StatusSnapshot {
        session_id: "private-child-id".into(),
        name: "worker".into(),
        output_path: "/synthetic/output.dat".into(),
        activity: AgentSessionActivitySnapshot {
            state: DrainState::Running,
            current_tool: Some("wait".into()),
            request_session_duration: Some(Duration::from_secs(2)),
            current_provider_request_duration: Some(Duration::from_secs(2)),
            last_provider_request_duration: None,
            latest_provider_activity_age: None,
            terminal_outcome: None,
            recent: vec![entry(1, "line one\nline two")].into_boxed_slice(),
        },
        children: vec![
            StatusChild {
                session_id: "z".into(),
                parent_session_id: "private-child-id".into(),
                name: "last".into(),
                active: true,
            },
            StatusChild {
                session_id: "a".into(),
                parent_session_id: "private-child-id".into(),
                name: "first".into(),
                active: true,
            },
            StatusChild {
                session_id: "foreign".into(),
                parent_session_id: "another-owner".into(),
                name: "hidden".into(),
                active: true,
            },
            StatusChild {
                session_id: "idle".into(),
                parent_session_id: "private-child-id".into(),
                name: "idle".into(),
                active: false,
            },
        ],
        processes: vec![StatusProcess {
            name: "compiler".into(),
            id: "scope/compiler".into(),
        }],
    }
}
fn entry(sequence: i64, content: &str) -> AgentSessionActivityEntrySnapshot {
    AgentSessionActivityEntrySnapshot {
        kind: AgentSessionActivityEntryKind::AssistantMessage,
        content: content.into(),
        age: Duration::from_millis(15),
        sequence,
    }
}

#[test]
fn status_reports_current_activity_without_canonical_ids() {
    let report = StatusFormatter::default().format(&snapshot());
    assert_eq!(
        report,
        "Agent status\nName: worker\nLive output file: /synthetic/output.dat\nLifecycle: running\nRequest session duration: 2.0s\nCurrent provider request duration: 2.0s\nCurrent tool: wait\nActive direct subagents:\n- first\n- last\nActive processes:\n- compiler (scope/compiler, shell, running)\nRecent entries:\n- assistant message (15ms ago): line one\n  line two"
    );
    assert!(!report.contains("private-child-id"));
    assert!(!report.contains("hidden"));
}

#[test]
fn recent_budget_and_alias_cursor_follow_original_sequence_rules() {
    let formatter = StatusFormatter::default();
    let mut current = snapshot();
    formatter.format(&current);
    current.activity.recent = vec![
        entry(1, "line one"),
        entry(2, &" a".repeat(600)),
        entry(3, &" b".repeat(600)),
    ]
    .into_boxed_slice();
    let first = formatter.format(&current);
    assert!(first.contains("2 older entries omitted to stay within 1K tokens"));
    assert!(first.contains(&" b".repeat(600)));
    assert!(!first.contains(&" a".repeat(600)));
    assert!(!first.contains("line one"));
    let repeat = formatter.format(&current);
    assert!(repeat.contains("2 earlier entries already reported by a previous agent_status"));
    assert!(!repeat.contains("older entries omitted"));
    let mut entries = current.activity.recent.to_vec();
    entries.push(entry(4, "fresh one"));
    entries.push(entry(5, "fresh two"));
    current.activity.recent = entries.into_boxed_slice();
    let alias = formatter.format(&current);
    assert!(alias.contains("2 earlier entries already reported"));
    assert!(alias.contains(&" b".repeat(600)));
    assert!(alias.contains("fresh one"));
    assert!(alias.contains("fresh two"));
    assert!(!alias.contains(&" a".repeat(600)));
}

#[test]
fn huge_newest_entry_and_utf16_token_budget_do_not_leak_older_entries() {
    let mut current = snapshot();
    current.activity.recent =
        vec![entry(1, "small"), entry(2, &"界".repeat(4001))].into_boxed_slice();
    let report = StatusFormatter::default().format(&current);
    assert!(report.contains("2 older entries omitted"));
    assert!(!report.contains("small"));
    assert!(!report.contains('界'));
    current.activity.recent = vec![entry(1, &"😀".repeat(2001))].into_boxed_slice();
    let report = StatusFormatter::default().format(&current);
    assert!(report.contains("1 older entries omitted"));
    assert!(!report.contains('😀'));
}

#[test]
fn status_empty_last_provider_age_and_terminal_branches() {
    let mut current = snapshot();
    current.activity.state = DrainState::Idle;
    current.activity.request_session_duration = None;
    current.activity.current_provider_request_duration = None;
    current.activity.last_provider_request_duration = Some(Duration::from_millis(1234));
    current.activity.current_tool = None;
    current.activity.latest_provider_activity_age = Some(Duration::from_millis(999));
    current.activity.terminal_outcome = Some(AgentExecution::canceled());
    current.activity.recent = vec![].into_boxed_slice();
    current.children.clear();
    current.processes.clear();
    let report = StatusFormatter::default().format(&current);
    assert!(report.contains(
        "Lifecycle: idle\nRequest session duration: none\nLast provider request duration: 1.2s"
    ));
    assert!(report.contains("Latest provider activity: 999ms ago\nLast outcome: canceled"));
    assert!(
        report.ends_with(
            "Active direct subagents: none\nActive processes: none\nRecent entries: none"
        )
    );
}
