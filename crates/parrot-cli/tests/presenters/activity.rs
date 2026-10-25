use std::sync::Arc;
use std::time::Duration;

use parrot_cli::presenters::tools::{
    exec_command::ExecCommandToolPresenter, generic::GenericToolPresenter,
};
use parrot_cli::presenters::{ActivityUpdate, RawActivityView, ToolPresenterRegistry};
use parrot_cli::terminal_frames::{LiveBufferRenderContext, ScrollbackRenderContext};
use parrot_domain::protocol::messages::{parrot as wire, process, queue};
use protobuf::Message;

fn event(owner: &str, payload: wire::event::Payload) -> wire::Event {
    wire::Event {
        agent_session_id: owner.into(),
        payload: Some(payload),
        ..Default::default()
    }
}
fn turn(owner: &str) -> wire::Event {
    event(
        owner,
        wire::event::Payload::TurnStarted(wire::TurnStarted::default()),
    )
}
fn child(owner: &str, name: &str, parent: &str) -> wire::Event {
    event(
        owner,
        wire::event::Payload::AgentStarted(wire::AgentStarted {
            name: name.into(),
            parent_agent_session_id: parent.into(),
            ..Default::default()
        }),
    )
}
fn ended(owner: &str) -> wire::Event {
    event(
        owner,
        wire::event::Payload::TurnEnded(wire::TurnEnded {
            finish_reason: "stop".into(),
            ..Default::default()
        }),
    )
}
fn reasoning(owner: &str, text: &str, summary: bool, completed: bool) -> wire::Event {
    event(
        owner,
        wire::event::Payload::ReasoningChunk(wire::ReasoningChunk {
            fragment: text.into(),
            kind: if summary {
                wire::ReasoningKind::REASONING_KIND_SUMMARY.into()
            } else {
                wire::ReasoningKind::REASONING_KIND_RAW.into()
            },
            completed,
            ..Default::default()
        }),
    )
}
fn phase(owner: &str) -> wire::Event {
    event(
        owner,
        wire::event::Payload::ProviderRequestPhaseChanged(
            wire::ProviderRequestPhaseChangedEvent::default(),
        ),
    )
}
fn live(view: &RawActivityView) -> String {
    view.snapshot()
        .unwrap()
        .iter()
        .flat_map(|item| {
            item.render(LiveBufferRenderContext {
                columns: 160,
                color: false,
            })
            .unwrap()
            .lines
        })
        .map(|line| line.text().to_owned())
        .collect::<Vec<_>>()
        .join("|")
}
fn commits(update: &ActivityUpdate) -> Vec<String> {
    update
        .commits
        .iter()
        .map(|item| {
            item.render(ScrollbackRenderContext {
                columns: 160,
                color: false,
                inline_diff: false,
                previous_packing: None,
            })
            .unwrap()
            .join("|")
        })
        .collect()
}
fn view() -> RawActivityView {
    RawActivityView::new(Arc::new(
        ToolPresenterRegistry::new(Vec::new(), Arc::new(GenericToolPresenter)).unwrap(),
    ))
}

#[test]
fn raw_reasoning_shows_a_counted_spinner_and_flushes_one_notice() {
    for owner in ["root", "child"] {
        for trigger in ["prepare", "completed", "summary"] {
            let mut view = view();
            view.observe(&turn("root")).unwrap();
            if owner == "child" {
                view.observe(&child("child", "worker", "root")).unwrap();
                view.observe(&turn("child")).unwrap();
            }
            view.observe(&reasoning(
                owner,
                "reasoning about the problem",
                false,
                false,
            ))
            .unwrap();
            view.observe(&reasoning(owner, "\x1b[2Jmore reasoning", false, false))
                .unwrap();
            let expected = 10;
            let prefix = if owner == "root" {
                "⠋ ".to_owned()
            } else {
                "  ⠋ [worker] ".to_owned()
            };
            assert!(
                live(&view).contains(&format!("{prefix}Thinking ({expected} tokens)…")),
                "{}",
                live(&view)
            );
            let flushed = match trigger {
                "prepare" => view.observe(&phase(owner)).unwrap(),
                "completed" => view.observe(&reasoning(owner, "", false, true)).unwrap(),
                _ => view
                    .observe(&reasoning(owner, "# Findings", true, false))
                    .unwrap(),
            };
            let actual = commits(&flushed);
            assert_eq!(actual.len(), 1);
            assert_eq!(
                actual[0],
                format!(
                    "{}✦ {}Reasoned for {expected} tokens…",
                    if owner == "root" { "" } else { "  " },
                    if owner == "root" { "" } else { "[worker] " }
                )
            );
            assert!(!live(&view).contains("Thinking ("));
        }
    }
}

#[test]
fn raw_reasoning_count_grows_with_each_fragment_and_stays_silent_when_nothing_arrived() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    assert!(commits(&view.observe(&phase("root")).unwrap()).is_empty());
    let empty = view.observe(&reasoning("root", "", true, false)).unwrap();
    assert!(commits(&empty).is_empty());
    assert!(!live(&view).contains("Thinking ("));
    for (fragment, expected) in [("first", 1), ("\x1b[2Jsecond", 5), (" third\n", 7)] {
        let update = view
            .observe(&reasoning("root", fragment, false, false))
            .unwrap();
        assert!(live(&view).contains(&format!("⠋ Thinking ({expected} tokens)…")));
        assert!(commits(&update).is_empty());
    }
}

#[test]
fn raw_reasoning_count_is_humanized_once_it_is_large() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&reasoning("root", &" a".repeat(4_100), false, false))
        .unwrap();
    assert!(live(&view).contains("⠋ Thinking (4.1k tokens)…"));
    assert_eq!(
        commits(&view.observe(&phase("root")).unwrap()),
        ["✦ Reasoned for 4.1k tokens…"]
    );
}

#[test]
fn raw_reasoning_state_is_isolated_per_agent() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&child("child", "worker", "root")).unwrap();
    view.observe(&turn("child")).unwrap();
    view.observe(&reasoning("root", "root reasoning", false, false))
        .unwrap();
    view.observe(&reasoning(
        "child",
        "a much longer child reasoning",
        false,
        false,
    ))
    .unwrap();
    assert!(live(&view).contains("⠋ Thinking (2 tokens)…"));
    assert!(live(&view).contains("  ⠋ [worker] Thinking (5 tokens)…"));
    let child = view.observe(&phase("child")).unwrap();
    assert_eq!(commits(&child).len(), 1);
    assert!(commits(&child)[0].contains("[worker]"));
    assert!(live(&view).contains("Thinking ("));
    assert!(!live(&view).contains("[worker] Thinking ("));
    let root = view.observe(&phase("root")).unwrap();
    assert_eq!(commits(&root).len(), 1);
    assert!(!commits(&root)[0].contains("[worker]"));
    assert!(!live(&view).contains("Thinking ("));
}

#[test]
fn encrypted_reasoning_commits_one_hierarchical_notice_per_event() {
    for owner in ["root", "child"] {
        let mut view = view();
        view.observe(&turn("root")).unwrap();
        if owner == "child" {
            view.observe(&child("child", "worker", "root")).unwrap();
            view.observe(&turn("child")).unwrap();
        }
        for _ in 0..2 {
            let update = view
                .observe(&event(
                    owner,
                    wire::event::Payload::EncryptedReasoningReceived(
                        wire::EncryptedReasoningReceivedEvent::default(),
                    ),
                ))
                .unwrap();
            assert_eq!(
                commits(&update),
                [if owner == "root" {
                    "✦ Reasoning received (encrypted)"
                } else {
                    "  ✦ [worker] Reasoning received (encrypted)"
                }]
            );
            assert!(!live(&view).contains("Thinking ("));
        }
    }
}

#[test]
fn encrypted_reasoning_notice_flushes_and_preserves_readable_summary() {
    for owner in ["root", "child"] {
        let mut view = view();
        view.observe(&turn("root")).unwrap();
        if owner == "child" {
            view.observe(&child("child", "worker", "root")).unwrap();
            view.observe(&turn("child")).unwrap();
        }
        view.observe(&reasoning(owner, "Readable summary", true, false))
            .unwrap();
        let update = view
            .observe(&event(
                owner,
                wire::event::Payload::EncryptedReasoningReceived(
                    wire::EncryptedReasoningReceivedEvent::default(),
                ),
            ))
            .unwrap();
        let actual = commits(&update);
        assert_eq!(actual.len(), 2);
        assert!(actual[0].contains("Readable summary"));
        assert!(actual[1].contains("Reasoning received (encrypted)"));
        assert_eq!(actual.join("|").matches("Readable summary").count(), 1);
    }
}

#[test]
fn encrypted_reasoning_notices_survive_retry_and_failure() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&child("child", "worker", "root")).unwrap();
    view.observe(&turn("child")).unwrap();
    let mut actual = Vec::new();
    for payload in [
        wire::event::Payload::EncryptedReasoningReceived(
            wire::EncryptedReasoningReceivedEvent::default(),
        ),
        wire::event::Payload::RetryNotice(wire::RetryNotice {
            attempt: 1,
            reason: "retrying".into(),
            ..Default::default()
        }),
        wire::event::Payload::EncryptedReasoningReceived(
            wire::EncryptedReasoningReceivedEvent::default(),
        ),
        wire::event::Payload::TurnFailed(wire::TurnFailed {
            message: "request failed".into(),
            ..Default::default()
        }),
    ] {
        actual.extend(commits(&view.observe(&event("child", payload)).unwrap()));
    }
    assert_eq!(actual.len(), 3);
    assert!(actual[0].contains("Reasoning received (encrypted)"));
    assert!(actual[1].contains("retry 1"));
    assert!(actual[2].contains("Reasoning received (encrypted)"));
    assert!(!actual.join("|").contains("tokens"));
}

fn task(
    name: &str,
    owner: &str,
    hidden: bool,
    status: wire::AgentTaskProgressStatus,
) -> wire::AgentTaskProgressNode {
    wire::AgentTaskProgressNode {
        name: name.into(),
        agent_session_id: owner.into(),
        hidden,
        status: status.into(),
        ..Default::default()
    }
}
fn progress(owner: &str, revision: u64, nodes: Vec<wire::AgentTaskProgressNode>) -> wire::Event {
    let snapshot = wire::AgentTaskProgressSnapshot {
        revision,
        root_nodes: nodes,
        ..Default::default()
    };
    event(
        owner,
        wire::event::Payload::ToolSnapshot(wire::ToolSnapshot {
            kind: "agent_task_progress".into(),
            owner_agent_session_id: owner.into(),
            inventory_instance_id: "tasks".into(),
            revision,
            final_chunk: true,
            data: snapshot.write_to_bytes().unwrap(),
            ..Default::default()
        }),
    )
}

#[test]
fn hidden_tasks_follow_worker_activity_without_new_progress_revisions() {
    for status in [
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING,
        wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED,
    ] {
        let mut view = view();
        view.observe(&turn("root")).unwrap();
        view.observe(&child("worker", "worker", "root")).unwrap();
        let original = task("hidden task", "worker", true, status);
        view.observe(&progress("root", 1, vec![original.clone()]))
            .unwrap();
        assert!(!live(&view).contains("Agent tasks:"));
        view.observe(&turn("worker")).unwrap();
        assert!(live(&view).contains("Agent tasks:"));
        assert!(live(&view).contains("hidden task"));
        assert_eq!(live(&view).matches("[worker]").count(), 1);
        view.observe(&reasoning("worker", "Still thinking", false, false))
            .unwrap();
        assert!(live(&view).contains("Thinking ("));
        view.observe(&event(
            "worker",
            wire::event::Payload::TextChunk(wire::TextChunk {
                fragment: "streamed response".into(),
                ..Default::default()
            }),
        ))
        .unwrap();
        assert!(live(&view).contains("streamed response"));
        let update = view.observe(&ended("worker")).unwrap();
        assert!(!live(&view).contains("Agent tasks:"));
        assert!(!live(&view).contains("hidden task"));
        assert!(commits(&update).join("|").contains("streamed response"));
        assert!(original.hidden);
    }
}

#[test]
fn nested_hidden_graphs_reveal_only_the_path_to_a_working_descendant() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&child("parent", "parent", "root")).unwrap();
    view.observe(&child("worker", "worker", "parent")).unwrap();
    view.observe(&progress(
        "root",
        1,
        vec![task(
            "hidden parent",
            "parent",
            true,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_UNSPECIFIED,
        )],
    ))
    .unwrap();
    view.observe(&progress(
        "parent",
        1,
        vec![
            task(
                "active child task",
                "worker",
                true,
                wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_UNSPECIFIED,
            ),
            task(
                "idle sibling",
                "idle",
                false,
                wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_UNSPECIFIED,
            ),
        ],
    ))
    .unwrap();
    assert!(!live(&view).contains("Agent tasks:"));
    view.observe(&turn("worker")).unwrap();
    let text = live(&view);
    assert!(text.contains("hidden parent"));
    assert!(text.contains("active child task"));
    assert!(!text.contains("idle sibling"));
    assert_eq!(text.matches("Agent tasks:").count(), 1);
    assert_eq!(text.matches("[worker]").count(), 1);
    view.observe(&ended("worker")).unwrap();
    assert!(!live(&view).contains("Agent tasks:"));
}

#[test]
fn hidden_successful_graph_flush_does_not_commit_an_empty_task_block() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&progress(
        "root",
        1,
        vec![task(
            "finished",
            "",
            true,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED,
        )],
    ))
    .unwrap();
    let finished = view.finish().unwrap();
    assert!(commits(&finished).is_empty());
    assert!(live(&view).is_empty());
    assert!(commits(&view.finish().unwrap()).is_empty());
}

#[test]
fn flushed_terminal_tree_retains_hidden_descendants_for_later_activity() {
    for separate_child_graph in [false, true] {
        let mut view = view();
        view.observe(&turn("root")).unwrap();
        view.observe(&child("worker", "worker", "root")).unwrap();
        let descendant = task(
            "hidden descendant",
            "worker",
            true,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED,
        );
        let mut ancestor = task(
            "visible ancestor",
            if separate_child_graph { "ancestor" } else { "" },
            false,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED,
        );
        if separate_child_graph {
            view.observe(&child("ancestor", "ancestor", "root"))
                .unwrap();
            view.observe(&progress("ancestor", 1, vec![descendant]))
                .unwrap();
        } else {
            ancestor.children.push(descendant);
        }
        view.observe(&progress("root", 1, vec![ancestor])).unwrap();
        let flushed = view.tick(Duration::from_secs(1)).unwrap();
        assert!(commits(&flushed).join("|").contains("visible ancestor"));
        view.observe(&turn("worker")).unwrap();
        assert!(live(&view).contains("visible ancestor"));
        assert!(live(&view).contains("hidden descendant"));
        view.observe(&ended("worker")).unwrap();
        assert!(!live(&view).contains("hidden descendant"));
    }
}

#[test]
fn terminal_tree_kept_for_active_worker_retires_after_activity_stops() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&child("worker", "worker", "root")).unwrap();
    view.observe(&turn("worker")).unwrap();
    view.observe(&progress(
        "root",
        1,
        vec![task(
            "finished task",
            "worker",
            false,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_SUCCEEDED,
        )],
    ))
    .unwrap();
    let flushed = view.tick(Duration::from_secs(1)).unwrap();
    assert!(commits(&flushed).join("|").contains("finished task"));
    view.observe(&ended("worker")).unwrap();
    assert!(!live(&view).contains("Agent tasks:"));
    assert!(!live(&view).contains("finished task"));
}

struct Processes {
    view: RawActivityView,
}
impl Processes {
    fn new() -> Self {
        let mut view = RawActivityView::new(Arc::new(
            ToolPresenterRegistry::new(
                vec![Arc::new(ExecCommandToolPresenter::new(Vec::new()))],
                Arc::new(GenericToolPresenter),
            )
            .unwrap(),
        ));
        view.observe(&turn("main")).unwrap();
        Self { view }
    }
    fn start(&mut self, id: &str, command: &str) -> ActivityUpdate {
        self.start_arguments(id, serde_json::json!({"command": command}).to_string())
    }
    fn start_arguments(&mut self, id: &str, arguments: String) -> ActivityUpdate {
        self.view
            .observe(&event(
                "main",
                wire::event::Payload::ToolCallChunk(wire::ToolCallChunk {
                    tool_call_id: id.into(),
                    tool_name: "exec_command".into(),
                    arguments_fragment: arguments,
                    ..Default::default()
                }),
            ))
            .unwrap();
        self.view
            .observe(&event(
                "main",
                wire::event::Payload::ToolStarted(wire::ToolStarted {
                    tool_call_id: id.into(),
                    tool_name: "exec_command".into(),
                    ..Default::default()
                }),
            ))
            .unwrap()
    }
    fn finish(
        &mut self,
        id: &str,
        name: &str,
        process: &str,
        instance: &str,
        revision: u64,
        monitor: bool,
    ) -> ActivityUpdate {
        self.view
            .observe(&event(
                "main",
                wire::event::Payload::ToolFinished(wire::ToolFinished {
                    tool_call_id: id.into(),
                    tool_name: "exec_command".into(),
                    yielded_process: Some(wire::YieldedShellProcess {
                        process_id: process.into(),
                        name: name.into(),
                        inventory_instance_id: instance.into(),
                        visible_revision: revision,
                        activity_kind: if monitor {
                            wire::ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR
                                .into()
                        } else {
                            wire::ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_EXECUTION
                                .into()
                        },
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                }),
            ))
            .unwrap()
    }
    fn yield_process(
        &mut self,
        id: &str,
        command: &str,
        name: &str,
        process: &str,
        instance: &str,
        revision: u64,
    ) -> ActivityUpdate {
        self.start(id, command);
        self.finish(id, name, process, instance, revision, false)
    }
    fn replace(
        &mut self,
        owner: &str,
        instance: &str,
        revision: u64,
        removed: bool,
        processes: Vec<process::ActiveShellProcess>,
        completed: Vec<process::CompletedShellProcess>,
    ) -> ActivityUpdate {
        self.view
            .replace_processes(
                &wire::ToolSnapshot {
                    owner_agent_session_id: owner.into(),
                    inventory_instance_id: instance.into(),
                    revision,
                    removed,
                    ..Default::default()
                },
                &process::ShellProcessSnapshot {
                    processes,
                    completed_processes: completed,
                    ..Default::default()
                },
            )
            .unwrap()
    }
    fn omit(&mut self, instance: &str, revision: u64) -> ActivityUpdate {
        self.replace("main", instance, revision, false, Vec::new(), Vec::new())
    }
}
fn process(id: &str, command: &str, owner: &str) -> process::ActiveShellProcess {
    process::ActiveShellProcess {
        process_id: id.into(),
        name: id.into(),
        command: command.into(),
        owner_agent_session_id: owner.into(),
        owner_agent_name: owner.into(),
        ..Default::default()
    }
}
fn tombstone(id: &str, elapsed: Option<i64>) -> process::CompletedShellProcess {
    process::CompletedShellProcess {
        process_id: id.into(),
        elapsed_ms: elapsed,
        ..Default::default()
    }
}

#[test]
fn yielded_exec_remains_live_without_committing() {
    let mut fixture = Processes::new();
    let update = fixture.yield_process("call", "sleep 20", "process", "process-1", "inventory", 1);
    assert!(commits(&update).is_empty());
    assert!(live(&fixture.view).contains("$ sleep 20 (process process running"));
}

#[test]
fn owner_inventory_replacement_and_removal_preserve_sibling_yielded_process() {
    let mut fixture = Processes::new();
    fixture.yield_process(
        "call",
        "sleep 20",
        "root-process",
        "root-process",
        "root-inventory",
        1,
    );
    let child = process::ActiveShellProcess {
        owner_agent_name: "worker".into(),
        parent_agent_session_id: "main".into(),
        depth: 1,
        ..process("child-process", "child command", "child")
    };
    fixture.replace(
        "child",
        "child-inventory",
        8,
        false,
        vec![child.clone()],
        vec![],
    );
    assert!(live(&fixture.view).contains("root-process running"));
    assert!(live(&fixture.view).contains("child-process running"));
    fixture.yield_process(
        "second-call",
        "sleep 30",
        "root-process",
        "root-process",
        "new-root-inventory",
        1,
    );
    assert!(live(&fixture.view).contains("root-process running"));
    assert!(live(&fixture.view).contains("child-process running"));
    fixture.replace(
        "child",
        "replacement",
        1,
        false,
        vec![child.clone()],
        vec![],
    );
    assert_eq!(
        commits(&fixture.replace("child", "replacement", 2, true, vec![], vec![])).len(),
        1
    );
    let ignored = fixture.replace("child", "child-inventory", 9, false, vec![child], vec![]);
    assert!(live(&fixture.view).contains("root-process running"));
    assert!(!live(&fixture.view).contains("child-process running"));
    assert!(commits(&ignored).is_empty());
    assert_eq!(
        commits(&fixture.omit("new-root-inventory", 2)),
        ["$ sleep 30"]
    );
}

#[test]
fn named_long_process_shows_name_and_description() {
    let mut fixture = Processes::new();
    let command = format!("echo {}", "x".repeat(200));
    fixture.start_arguments(
        "call",
        serde_json::json!({"command": command, "name":"build", "description":"Build it"})
            .to_string(),
    );
    let yielded = fixture.finish("call", "build", "process-1", "inventory", 1, false);
    assert_eq!(commits(&yielded).len(), 1);
    assert_eq!(commits(&yielded)[0].matches('x').count(), 200);
    assert!(live(&fixture.view).contains("$ build · Build it (process build running"));
    assert_eq!(
        commits(&fixture.replace(
            "main",
            "inventory",
            2,
            false,
            vec![],
            vec![tombstone("process-1", Some(7123))]
        )),
        ["$ build · Build it (7s)"]
    );
    fixture.replace(
        "main",
        "inventory",
        3,
        false,
        vec![process::ActiveShellProcess {
            name: "test".into(),
            description: "Test it".into(),
            ..process("process-2", &command, "main")
        }],
        vec![],
    );
    assert!(live(&fixture.view).contains("$ test · Test it (process test running"));
    assert_eq!(commits(&fixture.omit("inventory", 4)), ["$ test · Test it"]);
}

#[test]
fn origin_tool_completion_waits_for_terminal_output() {
    let mut fixture = Processes::new();
    fixture.start("call", "rg AgentTask");
    let active = process::ActiveShellProcess {
        origin_tool_call_id: "call".into(),
        ..process("process-1", "rg AgentTask", "main")
    };
    fixture.replace("main", "inventory", 1, false, vec![active], vec![]);
    assert!(!live(&fixture.view).contains("process process-1 running"));
    assert!(commits(&fixture.omit("inventory", 2)).is_empty());
    let finished = fixture
        .view
        .observe(&event(
            "main",
            wire::event::Payload::ToolFinished(wire::ToolFinished {
                tool_call_id: "call".into(),
                tool_name: "exec_command".into(),
                result: Some("matching output".into()),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(commits(&finished).len(), 1);
    assert!(commits(&finished)[0].contains("matching output"));
}

#[test]
fn terminal_output_suppresses_later_origin_process_completion() {
    let mut fixture = Processes::new();
    fixture.start("call", "rg AgentTask");
    fixture.replace(
        "main",
        "inventory",
        1,
        false,
        vec![process::ActiveShellProcess {
            origin_tool_call_id: "call".into(),
            ..process("process-1", "rg AgentTask", "main")
        }],
        vec![],
    );
    let finished = fixture
        .view
        .observe(&event(
            "main",
            wire::event::Payload::ToolFinished(wire::ToolFinished {
                tool_call_id: "call".into(),
                tool_name: "exec_command".into(),
                result: Some("matching output".into()),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(commits(&finished).len(), 1);
    assert!(commits(&finished)[0].contains("matching output"));
    assert!(commits(&fixture.omit("inventory", 2)).is_empty());
}

#[test]
fn newer_omission_commits_neutral_original_command() {
    let mut fixture = Processes::new();
    fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory", 1);
    let update = fixture.omit("inventory", 2);
    assert_eq!(commits(&update), ["$ sleep 20"]);
    assert!(!live(&fixture.view).contains("process build running"));
}

#[test]
fn completed_process_flushes_the_entire_multiline_command() {
    let mut fixture = Processes::new();
    let command = (1..=12)
        .map(|line| format!("echo line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    fixture.yield_process("call", &command, "build", "process-1", "inventory", 1);
    let actual = commits(&fixture.omit("inventory", 2));
    assert_eq!(actual.len(), 1);
    assert!(actual[0].starts_with("$ echo line-1|  echo line-2"));
    assert!(actual[0].ends_with("echo line-12"));
}

#[test]
fn snapshot_before_yielded_completion_commits_without_showing_process() {
    let mut fixture = Processes::new();
    fixture.start("call", "sleep 20");
    fixture.omit("inventory", 2);
    let update = fixture.finish("call", "build", "process-1", "inventory", 1, false);
    assert_eq!(commits(&update), ["$ sleep 20"]);
    assert!(!live(&fixture.view).contains("process build running"));
    assert!(
        commits(&fixture.finish("call", "build", "process-1", "inventory", 1, false)).is_empty()
    );
}

#[test]
fn completion_tombstone_correlates_after_yield() {
    let mut fixture = Processes::new();
    fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory", 1);
    assert_eq!(
        commits(&fixture.replace(
            "main",
            "inventory",
            2,
            false,
            vec![],
            vec![tombstone("process-1", Some(7123))]
        )),
        ["$ sleep 20 (7s)"]
    );
}

#[test]
fn retained_completion_tombstone_flushes_a_command_once() {
    let mut fixture = Processes::new();
    fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory", 1);
    assert_eq!(
        commits(&fixture.replace(
            "main",
            "inventory",
            2,
            false,
            vec![],
            vec![tombstone("process-1", Some(7123))]
        )),
        ["$ sleep 20 (7s)"]
    );
    assert!(
        commits(&fixture.replace(
            "main",
            "inventory",
            3,
            false,
            vec![],
            vec![tombstone("process-1", Some(7123))]
        ))
        .is_empty()
    );
}

#[test]
fn completion_tombstone_renders_duration_only_above_the_strict_threshold() {
    for (elapsed, expected) in [
        (None, "$ sleep 20"),
        (Some(4999), "$ sleep 20"),
        (Some(5000), "$ sleep 20"),
        (Some(5001), "$ sleep 20 (5s)"),
        (Some(65123), "$ sleep 20 (1m 05s)"),
        (Some(3723000), "$ sleep 20 (1h 02m 03s)"),
    ] {
        let mut fixture = Processes::new();
        fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory", 1);
        assert_eq!(
            commits(&fixture.replace(
                "main",
                "inventory",
                2,
                false,
                vec![],
                vec![tombstone("process-1", elapsed)]
            )),
            [expected]
        );
    }
}

#[test]
fn completion_tombstone_correlates_before_yield_without_replaying_history() {
    let mut fixture = Processes::new();
    assert!(
        commits(&fixture.replace(
            "main",
            "inventory",
            2,
            false,
            vec![],
            vec![
                tombstone("process-1", Some(8456)),
                tombstone("historical", Some(9000))
            ]
        ))
        .is_empty()
    );
    assert_eq!(
        commits(&fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory", 1)),
        ["$ sleep 20 (8s)"]
    );
    assert!(
        commits(&fixture.finish("call", "build", "process-1", "inventory", 1, false)).is_empty()
    );
}

#[test]
fn equal_and_stale_snapshots_do_not_complete_yielded_process() {
    let mut fixture = Processes::new();
    fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory", 2);
    assert!(commits(&fixture.omit("inventory", 2)).is_empty());
    let stale = fixture.omit("inventory", 1);
    assert!(commits(&stale).is_empty());
    assert!(live(&fixture.view).contains("$ sleep 20 (process build running"));
}

#[test]
fn reyielded_process_retains_its_original_command() {
    let mut fixture = Processes::new();
    fixture.yield_process(
        "first",
        "first command",
        "build",
        "process-1",
        "inventory",
        1,
    );
    fixture.yield_process(
        "second",
        "replacement command",
        "build",
        "process-1",
        "inventory",
        2,
    );
    let actual = commits(&fixture.omit("inventory", 3));
    assert_eq!(actual.len(), 1);
    assert!(actual[0].contains("first command"));
    assert!(!actual[0].contains("replacement command"));
}

#[test]
fn reconnected_inventory_omission_completes_existing_process() {
    let mut fixture = Processes::new();
    fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory-a", 1);
    fixture.replace(
        "main",
        "inventory-b",
        1,
        false,
        vec![process("process-1", "reported command", "main")],
        vec![],
    );
    let actual = commits(&fixture.omit("inventory-b", 2));
    assert_eq!(actual, ["$ reported command"]);
}

#[test]
fn snapshot_origin_completion_uses_reported_owner_hierarchy() {
    let mut fixture = Processes::new();
    fixture.replace(
        "opaque-child-id",
        "inventory",
        1,
        false,
        vec![process::ActiveShellProcess {
            owner_agent_name: "worker".into(),
            parent_agent_session_id: "opaque-parent-id".into(),
            parent_agent_name: "parent".into(),
            depth: 2,
            ..process("process-1", "reported command", "opaque-child-id")
        }],
        vec![],
    );
    let actual =
        commits(&fixture.replace("opaque-child-id", "inventory", 2, false, vec![], vec![]));
    assert_eq!(actual, ["    $ [worker] reported command"]);
}

#[test]
fn inventory_replacement_does_not_complete_missing_old_process() {
    let mut fixture = Processes::new();
    fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory-a", 1);
    assert!(commits(&fixture.omit("inventory-b", 1)).is_empty());
}

#[test]
fn duplicate_omissions_commit_process_once() {
    let mut fixture = Processes::new();
    fixture.yield_process("call", "sleep 20", "build", "process-1", "inventory", 1);
    assert_eq!(commits(&fixture.omit("inventory", 2)).len(), 1);
    assert!(commits(&fixture.omit("inventory", 3)).is_empty());
}

#[test]
fn multiple_omissions_commit_in_process_id_order() {
    let mut fixture = Processes::new();
    fixture.yield_process(
        "second",
        "second command",
        "second",
        "process-b",
        "inventory",
        1,
    );
    fixture.yield_process(
        "first",
        "first command",
        "first",
        "process-a",
        "inventory",
        1,
    );
    assert_eq!(
        commits(&fixture.omit("inventory", 2)),
        ["$ first command", "$ second command"]
    );
}

#[test]
fn preamble_end_retires_processes_without_a_live_inventory() {
    let mut fixture = Processes::new();
    fixture.yield_process(
        "call",
        "sleep 20",
        "stale-process",
        "stale-process",
        "previous-run",
        1,
    );
    fixture.replace(
        "child",
        "child-inventory",
        1,
        false,
        vec![process::ActiveShellProcess {
            owner_agent_name: "worker".into(),
            parent_agent_session_id: "main".into(),
            depth: 1,
            ..process("live-process", "live command", "child")
        }],
        vec![],
    );
    let update = fixture.view.retire_unconfirmed_processes().unwrap();
    assert!(!live(&fixture.view).contains("stale-process running"));
    assert!(live(&fixture.view).contains("live-process running"));
    assert!(commits(&update).is_empty());
    assert!(
        commits(&fixture.finish(
            "call",
            "stale-process",
            "stale-process",
            "previous-run",
            1,
            false
        ))
        .is_empty()
    );
}

#[test]
fn monitor_marker_blinks_without_moving_or_restyling_content() {
    for (frame, visible) in [(0, true), (5, true), (6, false), (11, false), (12, true)] {
        let mut fixture = Processes::new();
        fixture.start("call", "monitor command");
        fixture.finish("call", "watch", "process-1", "inventory", 1, true);
        fixture
            .view
            .tick(Duration::from_millis(frame * 80))
            .unwrap();
        let process = fixture
            .view
            .snapshot()
            .unwrap()
            .iter()
            .find_map(|item| {
                let rendered = item
                    .render(LiveBufferRenderContext {
                        columns: 160,
                        color: true,
                    })
                    .unwrap();
                rendered
                    .lines
                    .iter()
                    .any(|line| line.text().contains("process watch running"))
                    .then_some(rendered)
            })
            .unwrap();
        assert_eq!(process.lines[0].text().starts_with("◉ "), visible);
        assert!(
            process.lines[0]
                .text()
                .contains("$ monitor command (process watch running")
        );
        assert_eq!(process.lines[0].style().start(), "\x1b[48;5;236m\x1b[36m");
        assert_eq!(
            process.retention,
            parrot_cli::terminal_frames::LiveBufferRetention::Fixed
        );
    }
}

#[test]
fn non_monitor_kind_preserves_spinner() {
    for kind in [0, 99] {
        let mut fixture = Processes::new();
        fixture.replace(
            "main",
            "inventory",
            1,
            false,
            vec![process::ActiveShellProcess {
                activity_kind: protobuf::EnumOrUnknown::from_i32(kind),
                ..process("process-1", "command", "main")
            }],
            vec![],
        );
        assert!(live(&fixture.view).starts_with("⠋ $ command"));
    }
}

#[test]
fn monitor_kind_survives_either_event_order_and_completion() {
    for inventory_first in [false, true] {
        let mut fixture = Processes::new();
        fixture.start("call", "monitor command");
        let inventory_process = process::ActiveShellProcess {
            name: "watch".into(),
            activity_kind: wire::ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR
                .into(),
            ..process("process-1", "monitor command", "main")
        };
        if inventory_first {
            fixture.replace(
                "main",
                "inventory",
                1,
                false,
                vec![inventory_process.clone()],
                vec![],
            );
        }
        fixture.finish("call", "watch", "process-1", "inventory", 1, true);
        assert!(live(&fixture.view).contains("◉ $ monitor command"));
        if !inventory_first {
            fixture.replace(
                "main",
                "inventory",
                2,
                false,
                vec![inventory_process],
                vec![],
            );
            assert!(live(&fixture.view).contains("◉ $ monitor command"));
        }
        let completed = fixture.omit("inventory", 3);
        assert_eq!(commits(&completed), ["$ monitor command"]);
        assert!(!live(&fixture.view).contains("watch running"));
    }
}

#[test]
fn inventory_only_monitor_and_exec_use_distinct_markers_and_remove_cleanly() {
    let mut fixture = Processes::new();
    fixture.replace(
        "main",
        "inventory",
        1,
        false,
        vec![
            process::ActiveShellProcess {
                activity_kind: wire::ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR
                    .into(),
                ..process("monitor", "watch command", "main")
            },
            process("exec", "exec command", "main"),
        ],
        vec![],
    );
    assert!(live(&fixture.view).contains("◉ $ watch command"));
    assert!(live(&fixture.view).contains("⠋ $ exec command"));
    let update = fixture.omit("inventory", 2);
    assert_eq!(commits(&update).len(), 2);
    assert!(!live(&fixture.view).contains("running"));
}

#[test]
fn queue_inventory_replacement_is_owner_scoped_and_fences_retired_instances() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    let snapshot = |owner: &str, name: &str, count| queue::QueueSnapshot {
        root_agent_session_id: "root".into(),
        queues: vec![queue::QueueState {
            name: name.into(),
            item_count: count,
            owner_agent_session_id: owner.into(),
            owner_agent_name: owner.into(),
            parent_agent_session_id: if owner == "root" { "" } else { "root" }.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let inventory = |owner: &str, instance: &str, revision, removed| wire::ToolSnapshot {
        owner_agent_session_id: owner.into(),
        inventory_instance_id: instance.into(),
        revision,
        removed,
        ..Default::default()
    };
    view.replace_queues(
        &inventory("root", "root-inventory", 1, false),
        &snapshot("root", "root-work", 2),
    )
    .unwrap();
    view.replace_queues(
        &inventory("child", "child-inventory", 1, false),
        &snapshot("child", "child-work", 1),
    )
    .unwrap();
    assert!(live(&view).contains("queue: root-work · 2 items"));
    assert!(live(&view).contains("[child] queue: child-work · 1 item"));
    view.replace_queues(
        &inventory("child", "replacement", 1, false),
        &snapshot("child", "new-work", 3),
    )
    .unwrap();
    view.replace_queues(
        &inventory("child", "child-inventory", 2, false),
        &snapshot("child", "stale-work", 9),
    )
    .unwrap();
    assert!(live(&view).contains("new-work"));
    assert!(!live(&view).contains("stale-work"));
    view.replace_queues(
        &inventory("child", "replacement", 2, true),
        &queue::QueueSnapshot::default(),
    )
    .unwrap();
    assert!(live(&view).contains("root-work"));
    assert!(!live(&view).contains("new-work"));
    view.replace_queues(
        &inventory("child", "replacement", 3, false),
        &snapshot("child", "resurrected", 4),
    )
    .unwrap();
    assert!(!live(&view).contains("resurrected"));
}

#[test]
fn returned_scrollback_owns_stable_data_until_actual_driver_acknowledges_it() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&reasoning("root", "first reasoning", false, false))
        .unwrap();
    let pending = view.observe(&phase("root")).unwrap();
    assert_eq!(commits(&pending), ["✦ Reasoned for 2 tokens…"]);
    assert!(commits(&view.observe(&phase("root")).unwrap()).is_empty());
    view.observe(&reasoning("root", "new reasoning state", false, false))
        .unwrap();
    assert_eq!(commits(&pending), ["✦ Reasoned for 2 tokens…"]);
    // Actual I/O retry/partial-publication acknowledgement belongs to T72's
    // driver. This assertion is ownership/orchestration evidence, not that gate.
}

#[test]
fn task_progress_quiet_flush_coalesces_revisions_and_shutdown_flushes_once() {
    let mut view = view();
    view.observe(&turn("root")).unwrap();
    view.observe(&progress(
        "root",
        1,
        vec![task(
            "old task",
            "",
            false,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING,
        )],
    ))
    .unwrap();
    assert!(commits(&view.tick(Duration::from_millis(700)).unwrap()).is_empty());
    view.observe(&progress(
        "root",
        2,
        vec![task(
            "latest task",
            "",
            false,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING,
        )],
    ))
    .unwrap();
    assert!(commits(&view.tick(Duration::from_millis(1000)).unwrap()).is_empty());
    let update = view.tick(Duration::from_millis(1700)).unwrap();
    assert_eq!(commits(&update).len(), 1);
    assert!(commits(&update)[0].contains("latest task"));
    assert!(!commits(&update)[0].contains("old task"));
    assert!(commits(&view.tick(Duration::from_secs(2)).unwrap()).is_empty());
    view.observe(&progress(
        "root",
        3,
        vec![task(
            "last task",
            "",
            false,
            wire::AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_RUNNING,
        )],
    ))
    .unwrap();
    let shutdown = view.finish().unwrap();
    assert_eq!(commits(&shutdown).len(), 1);
    assert!(commits(&shutdown)[0].contains("last task"));
    assert!(commits(&view.finish().unwrap()).is_empty());
    assert!(commits(&view.tick(Duration::from_secs(10)).unwrap()).is_empty());
}

#[test]
fn deferred_started_command_commits_after_grace_or_yield_and_omits_fast_start() {
    let command = format!("echo {}", "x".repeat(201));
    let args =
        serde_json::json!({"command":command,"name":"build","description":"Build it"}).to_string();
    let mut fixture = Processes::new();
    assert!(commits(&fixture.start_arguments("fast", args.clone())).is_empty());
    assert!(commits(&fixture.view.tick(Duration::from_millis(1999)).unwrap()).is_empty());
    let finished = fixture
        .view
        .observe(&event(
            "main",
            wire::event::Payload::ToolFinished(wire::ToolFinished {
                tool_call_id: "fast".into(),
                tool_name: "exec_command".into(),
                result: Some("done".into()),
                ..Default::default()
            }),
        ))
        .unwrap();
    assert_eq!(commits(&finished).len(), 1);
    assert_eq!(commits(&finished)[0].matches('x').count(), 201);
    assert!(commits(&fixture.view.tick(Duration::from_secs(2)).unwrap()).is_empty());
    fixture.start_arguments("slow", args.clone());
    let started = fixture.view.tick(Duration::from_secs(4)).unwrap();
    assert_eq!(commits(&started).len(), 1);
    assert_eq!(commits(&started)[0].matches('x').count(), 201);
    assert!(commits(&fixture.view.tick(Duration::from_secs(5)).unwrap()).is_empty());
    fixture.start_arguments("yielded", args);
    let yielded = fixture.finish("yielded", "build", "process-1", "inventory", 1, false);
    assert_eq!(commits(&yielded).len(), 1);
    assert_eq!(commits(&yielded)[0].matches('x').count(), 201);
}
