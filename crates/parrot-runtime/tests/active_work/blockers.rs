use super::templates;
use parrot_domain::{
    contracts::{
        async_ops::BoxFuture,
        status::{
            ActiveWorkObservation, ActiveWorkState, ProcessStatusSnapshot, ProcessStatusSource,
        },
    },
    values::agent_tasks::{ExecutionStatus, Task, TaskPayload},
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_runtime::{
    active_work::{blockers::*, wait::*},
    agent_turn::services::TurnError,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

struct Recording {
    id: &'static str,
    observed: Arc<Mutex<Vec<&'static str>>>,
    result: Option<BlockerResult>,
}
impl ActiveWorkBlocker for Recording {
    fn observe(&self) -> BoxFuture<'_, Result<Option<BlockerResult>, TurnError>> {
        Box::pin(async move {
            self.observed.lock().unwrap().push(self.id);
            Ok(self.result.clone())
        })
    }
}
fn recording(
    id: &'static str,
    observed: &Arc<Mutex<Vec<&'static str>>>,
    section: Option<&str>,
    reminder: Option<&str>,
) -> Arc<dyn ActiveWorkBlocker> {
    Arc::new(Recording {
        id,
        observed: observed.clone(),
        result: Some(BlockerResult {
            work_section: section.map(str::to_owned),
            reminder: reminder.map(str::to_owned),
        }),
    })
}
fn custom_templates() -> Arc<PromptTemplateCatalog> {
    Arc::new(PromptTemplateCatalog::from_content(
        "prompt_templates:\n  agent-session.active-work-reminder:\n    template: 'agent-session.active-work-reminder[active_work={active_work}]'\n",
        &BTreeMap::from([("HOME".into(), "/synthetic-t52-home".into())]),
    ).unwrap().1)
}
#[tokio::test]
async fn reminder_observes_once_preserves_order_and_keeps_empty_results_blocking() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let reminder = ActiveWorkReminder {
        blockers: vec![
            recording("first", &observed, Some("\nfirst"), Some("first reminder")),
            recording(
                "second",
                &observed,
                Some("\nsecond"),
                Some("second reminder"),
            ),
        ],
        templates: custom_templates(),
    };
    assert_eq!(
        reminder.build().await.unwrap().as_deref(),
        Some(
            "agent-session.active-work-reminder[active_work=\nfirst\nsecond]\nfirst reminder\nsecond reminder"
        )
    );
    assert_eq!(*observed.lock().unwrap(), ["first", "second"]);
    let empty = ActiveWorkReminder {
        blockers: vec![recording("empty", &observed, Some(""), None)],
        templates: custom_templates(),
    };
    assert_eq!(
        empty.build().await.unwrap().as_deref(),
        Some("agent-session.active-work-reminder[active_work=]")
    );
    let empty = ActiveWorkReminder {
        blockers: vec![recording("empty", &observed, None, None)],
        templates: custom_templates(),
    };
    assert_eq!(empty.build().await.unwrap().as_deref(), Some(""));
    let empty = ActiveWorkReminder {
        blockers: vec![Arc::new(Recording {
            id: "none",
            observed,
            result: None,
        })],
        templates: templates(),
    };
    assert_eq!(empty.build().await.unwrap(), None);
}
#[tokio::test]
async fn wait_observes_only_work_sections_once_and_does_not_include_queue_reminders() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let reminder = ActiveWorkReminder {
        blockers: vec![
            recording(
                "process",
                &observed,
                Some("\nRunning processes:\n- process"),
                None,
            ),
            recording(
                "queue",
                &observed,
                None,
                Some("queue still blocks completion"),
            ),
        ],
        templates: templates(),
    };
    assert_eq!(
        reminder.work_sections().await.unwrap(),
        "\nRunning processes:\n- process"
    );
    assert_eq!(*observed.lock().unwrap(), ["process", "queue"]);
    assert!(
        reminder
            .build()
            .await
            .unwrap()
            .unwrap()
            .ends_with("queue still blocks completion")
    );
}
struct Processes(Mutex<Vec<ActiveWorkObservation>>);
impl ProcessStatusSource for Processes {
    fn active(&self) -> Vec<ActiveWorkObservation> {
        self.0.lock().unwrap().clone()
    }
    fn snapshot(&self) -> Vec<ProcessStatusSnapshot> {
        vec![]
    }
}
fn process(id: &str, name: &str) -> ActiveWorkObservation {
    ActiveWorkObservation {
        id: id.into(),
        name: name.into(),
        state: ActiveWorkState::Running,
    }
}
#[tokio::test]
async fn process_blocker_sorts_injected_domain_observations_and_resamples_without_clearing() {
    let source = Arc::new(Processes(Mutex::new(vec![
        process("process-b", "B"),
        process("process-a", "A"),
    ])));
    let blocker = ProcessBlocker {
        processes: source.clone(),
    };
    let first = blocker.observe().await.unwrap().unwrap();
    assert_eq!(
        first.work_section.as_deref(),
        Some("\nRunning processes:\n- process-a (name: A)\n- process-b (name: B)")
    );
    assert_eq!(first.reminder, None);
    assert_eq!(blocker.observe().await.unwrap(), Some(first));
    assert_eq!(source.0.lock().unwrap().len(), 2);
    source.0.lock().unwrap().clear();
    assert_eq!(blocker.observe().await.unwrap(), None);
    assert_eq!(format_section("unused", &[]), "");
}
struct Tasks(Mutex<Vec<Task>>);
impl TaskSnapshotSource for Tasks {
    fn snapshot(&self) -> BoxFuture<'_, Vec<Task>> {
        Box::pin(async { self.0.lock().unwrap().clone() })
    }
}
fn task(name: &str, state: ExecutionStatus) -> Task {
    Task {
        name: name.into(),
        dependencies: vec![],
        description: format!("Do {name}"),
        payload: TaskPayload::Instruction("work".into()),
        acceptance_criteria: "done".into(),
        model: None,
        hidden: true,
        state,
        result: None,
        failure: None,
    }
}
#[tokio::test]
async fn tasks_pending_running_and_failed_block_even_when_hidden_until_explicitly_resolved() {
    use ExecutionStatus::*;
    let source = Arc::new(Tasks(Mutex::new(vec![
        task("task-d", Running),
        task("task-b", Succeeded),
        task("task-c", Failed),
        task("task-e", Canceled),
        task("task-a", Pending),
    ])));
    let blocker = TaskBlocker {
        tasks: source.clone(),
        templates: templates(),
    };
    assert_eq!(blocker.observe().await.unwrap().unwrap(), BlockerResult {
        work_section: Some("\nUnresolved AgentTasks (pending, running, or failed and awaiting your set_agent_tasks decision):\n- task-a [pending] (name: Do task-a)\n- task-c [failed] (name: Do task-c)\n- task-d [running] (name: Do task-d)".into()),
        reminder: None,
    });
    assert_eq!(
        source
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|task| task.state == Failed)
            .count(),
        1
    );
    assert!(blocker.observe().await.unwrap().is_some());
    *source.0.lock().unwrap() = vec![task("done", Succeeded), task("dropped", Canceled)];
    assert_eq!(blocker.observe().await.unwrap(), None);
}
#[tokio::test]
async fn task_sections_follow_source_invariant_utf16_order_for_non_bmp_names() {
    let source = Arc::new(Tasks(Mutex::new(vec![
        task("\u{e000}", ExecutionStatus::Pending),
        task("\u{10000}", ExecutionStatus::Pending),
    ])));
    let blocker = TaskBlocker {
        tasks: source,
        templates: templates(),
    };
    assert_eq!(
        blocker
            .observe()
            .await
            .unwrap()
            .unwrap()
            .work_section
            .as_deref(),
        Some(
            "\nUnresolved AgentTasks (pending, running, or failed and awaiting your set_agent_tasks decision):\n- \u{10000} [pending] (name: Do \u{10000})\n- \u{e000} [pending] (name: Do \u{e000})"
        )
    );
    assert_eq!(
        format_section(
            "Running processes",
            &[process("\u{e000}", "BMP"), process("\u{10000}", "astral")]
        ),
        "\nRunning processes:\n- \u{10000} (name: astral)\n- \u{e000} (name: BMP)"
    );
}

#[test]
fn wait_argument_boundaries_match_original_rows_and_default() {
    for arguments in [
        "[]",
        "{\"unexpected\":true}",
        "{\"duration_ms\":9999}",
        "{\"duration_ms\":-1}",
        "{\"duration_ms\":10000.5}",
        "{\"duration_ms\":\"10000\"}",
        "{\"duration_ms\":4294967295}",
        "{\"duration_ms\":null}",
        "{",
        "{\"duration_ms\":10000.0}",
    ] {
        assert!(WaitDuration::parse(arguments).is_err(), "{arguments}");
    }
    assert_eq!(WaitDuration::parse("{}").unwrap().milliseconds(), 300_000);
    assert_eq!(
        WaitDuration::parse("{\"duration_ms\":10000}")
            .unwrap()
            .milliseconds(),
        10_000
    );
    assert_eq!(
        WaitDuration::parse("{\"duration_ms\":4294967294}")
            .unwrap()
            .milliseconds(),
        4_294_967_294
    );
    assert!(WaitDuration::new(0).is_none());
}
