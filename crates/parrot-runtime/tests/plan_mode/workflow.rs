use super::support::*;
use parrot_domain::{
    protocol::messages::parrot::AgentTaskProgressStatus,
    values::agent_tasks::{ExecutionStatus, TaskArtifact, TaskPayload},
};
use parrot_runtime::plan_mode::*;
use std::fs;

#[test]
fn completion_waits_for_prepare_and_fresh_owner_does_not_revive_prior_artifacts() {
    let fixture = Fixture::new();
    let (first, parser) = fixture.workflow();
    assert_eq!(first.complete().unwrap(), ModeCompletionOutcome::None);
    first.prepare().unwrap();
    write_valid(&first, &parser);
    assert!(matches!(
        first.complete().unwrap(),
        ModeCompletionOutcome::Completed(_)
    ));
    let (fresh, _) = fixture.workflow();
    assert_eq!(fresh.complete().unwrap(), ModeCompletionOutcome::None);
    fresh.prepare().unwrap();
    assert_ne!(fresh.artifacts().paths(), first.artifacts().paths());
    assert!(matches!(
        fresh.complete().unwrap(),
        ModeCompletionOutcome::Repair(_)
    ));
}

#[test]
fn completion_trims_reads_and_emits_exact_approval_dialog_without_authorizing_implementation() {
    let fixture = Fixture::new();
    let (workflow, parser) = fixture.workflow();
    workflow.prepare().unwrap();
    write_valid(&workflow, &parser);
    let paths = workflow.artifacts().paths().unwrap();
    fs::write(paths.plan(), " \n# Approved plan\n\t").unwrap();
    fs::write(paths.tasks(), format!(" \r\n{VALID}\n ")).unwrap();
    let ModeCompletionOutcome::Completed(completed) = workflow.complete().unwrap() else {
        panic!("completion missing")
    };
    assert_eq!(completed.markdown, "# Approved plan");
    assert_eq!(*parser.inputs.lock().unwrap(), [VALID]);
    let dialog = completed.dialog.as_ref().unwrap();
    assert_eq!(dialog.prompt, "Plan complete: ");
    assert_eq!(dialog.context, ["Review the plan before implementation."]);
    assert_eq!(dialog.choices.len(), 2);
    let yes = &dialog.choices[0];
    assert_eq!(yes.value, "yes");
    assert_eq!(yes.aliases, ["y"]);
    let action = yes.action.as_ref().unwrap();
    assert_eq!(action.mode, BUILD);
    assert_eq!(
        action.prompt,
        format!(
            "Implement the approved Markdown plan at {}. Call set_agent_tasks with the approved task JSON path {}.",
            paths.plan().display(),
            paths.tasks().display()
        )
    );
    assert_eq!(yes.description, "Implement the approved plan");
    assert_eq!(dialog.choices[1].description, "Stop after planning");
    assert_eq!(dialog.choices[1].value, "no");
    assert_eq!(dialog.choices[1].aliases, ["n"]);
    assert!(dialog.choices[1].action.is_none());
    assert_eq!(dialog.custom_choice, "feedback");
    assert_eq!(
        dialog.custom_description,
        "Provide feedback and revise the plan"
    );
    assert_eq!(dialog.custom_prompt, "plan feedback: ");
    assert_eq!(dialog.empty_message, "enter yes, no, or feedback");
    assert_eq!(workflow.artifacts().paths().unwrap(), paths);
}

#[test]
fn completion_projects_ordered_multi_root_nested_tasks_as_pending_declarations_and_progress() {
    let fixture = Fixture::new();
    let (workflow, parser) = fixture.workflow();
    workflow.prepare().unwrap();
    let mut child = task(
        "child",
        TaskPayload::Instruction("child instruction".into()),
    );
    child.state = ExecutionStatus::Succeeded;
    child.result = Some("earlier result".into());
    child.hidden = true;
    child.model = Some("small".into());
    let grandchild = task(
        "grandchild",
        TaskPayload::Instruction("grandchild instruction".into()),
    );
    let other_child = task("other-child", TaskPayload::Tasks(vec![grandchild]));
    let first = task("first", TaskPayload::Tasks(vec![child, other_child]));
    let mut second = task(
        "second",
        TaskPayload::Instruction("second instruction".into()),
    );
    second.dependencies = vec!["first".into()];
    second.state = ExecutionStatus::Failed;
    second.failure = Some("earlier failure".into());
    let paths = workflow.artifacts().paths().unwrap();
    fs::write(paths.plan(), "# Nested").unwrap();
    fs::write(paths.tasks(), "nested fixture").unwrap();
    parser.answer(
        "nested fixture",
        Ok(TaskArtifact {
            schema_version: 1,
            tasks: vec![first, second],
        }),
    );
    let ModeCompletionOutcome::Completed(completed) = workflow.complete().unwrap() else {
        panic!("completion missing")
    };
    assert_eq!(completed.task_tree.as_ref().unwrap().revision, 0);
    let nodes = &completed.task_tree.as_ref().unwrap().root_nodes;
    assert_eq!(
        nodes[0]
            .children
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        ["child", "other-child"]
    );
    let grandchild = &nodes[0].children[1].children[0];
    assert_eq!(grandchild.name, "grandchild");
    assert_eq!(
        grandchild.status.enum_value().unwrap(),
        AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING
    );
    assert_eq!(
        nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    let pending = AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING;
    for node in [&nodes[0], &nodes[0].children[0], &nodes[1]] {
        assert_eq!(node.status.enum_value().unwrap(), pending);
    }
    assert!(nodes[0].children[0].hidden);
    let declarations = &completed.task_declarations;
    assert_eq!(
        declarations
            .iter()
            .map(|task| task.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(declarations[0].description, "first");
    assert_eq!(declarations[0].acceptance_criteria, "Pass");
    assert_eq!(
        declarations[0]
            .children()
            .tasks
            .iter()
            .map(|task| task.name.as_str())
            .collect::<Vec<_>>(),
        ["child", "other-child"]
    );
    assert_eq!(
        declarations[0].children().tasks[1].children().tasks[0].instruction(),
        "grandchild instruction"
    );
    assert_eq!(declarations[1].dependencies, ["first"]);
    assert_eq!(declarations[1].instruction(), "second instruction");
    let child = &declarations[0].children().tasks[0];
    assert_eq!(child.name, "child");
    assert_eq!(child.model.as_deref(), Some("small"));
    assert!(child.hidden);
    assert_eq!(child.instruction(), "child instruction");
    for declaration in [&declarations[0], child, &declarations[1]] {
        assert_eq!(declaration.status.enum_value().unwrap(), pending);
    }
}

#[test]
fn blank_missing_unreadable_and_invalid_inputs_require_repair_without_partial_completion() {
    let fixture = Fixture::new();
    let (workflow, parser) = fixture.workflow();
    workflow.prepare().unwrap();
    let paths = workflow.artifacts().paths().unwrap();
    let repair = || match workflow.complete().unwrap() {
        ModeCompletionOutcome::Repair(text) => {
            assert!(!text.trim().is_empty());
            text
        }
        other => panic!("expected repair, got {other:?}"),
    };
    assert!(repair().contains(paths.plan().to_str().unwrap()));
    fs::write(paths.plan(), " \t\r\n ").unwrap();
    repair();
    assert!(parser.inputs.lock().unwrap().is_empty());
    fs::write(paths.plan(), "# Good").unwrap();
    assert!(repair().contains(paths.tasks().to_str().unwrap()));
    fs::write(paths.tasks(), " invalid ").unwrap();
    parser.answer(
        "invalid",
        Err(ParseFailure("scripted task validation rejection".into())),
    );
    let invalid = repair();
    assert!(invalid.contains("scripted task validation rejection"));
    assert_eq!(*parser.inputs.lock().unwrap(), ["invalid"]);
    fs::remove_file(paths.tasks()).unwrap();
    let missing = repair();
    assert!(missing.contains(paths.plan().to_str().unwrap()));
    assert!(missing.contains(paths.tasks().to_str().unwrap()));
    assert!(workflow.prepare().is_err());
    assert_eq!(workflow.artifacts().paths().unwrap(), paths);
    fs::create_dir(paths.tasks()).unwrap();
    repair();
    assert_eq!(*parser.inputs.lock().unwrap(), ["invalid"]);
}

#[tokio::test]
async fn real_history_checkpoint_does_not_serialize_or_revive_the_workflow_owner_pair() {
    use parrot_storage::{
        conversation::{ConversationMessage, ConversationRepository, MessageRole, ToolCall},
        database::SessionDatabase,
        events::{EventRepository, projection},
        history_files::AgentHistoryFile,
    };
    use std::sync::Arc;
    let fixture = Fixture::new();
    let scratch = fixture.scratch("main");
    let root = fixture.resources.selected_root().clone();
    let repository = ConversationRepository::new(EventRepository::new(
        SessionDatabase::open(root.clone()).unwrap(),
    ));
    let history = repository
        .bind_agent_history(
            "main".into(),
            Arc::new(
                AgentHistoryFile::new(root, scratch.history_path(), vec!["main".into()]).unwrap(),
            ),
        )
        .unwrap();
    let (workflow, parser) = fixture.workflow();
    workflow.prepare().unwrap();
    write_valid(&workflow, &parser);
    let paths = workflow.artifacts().paths().unwrap();
    let mut assistant =
        ConversationMessage::text(MessageRole::Assistant, "checkpoint current plan");
    assistant.tool_calls.push(ToolCall {
        id: "checkpoint-call".into(),
        name: "set_checkpoint".into(),
        arguments_json: "{\"title\":\"plan checkpoint\"}".into(),
    });
    let sequence = repository
        .events()
        .transaction(move |tx| {
            Ok(projection::project(tx, "main", &assistant, "2026-10-19T00:00:00Z")?.sequence)
        })
        .await
        .unwrap();
    assert!(
        history
            .record_checkpoint("plan checkpoint".into(), sequence, "checkpoint-call".into())
            .await
            .unwrap()
    );
    assert!(
        history
            .record_checkpoint("plan checkpoint".into(), sequence, "checkpoint-call".into())
            .await
            .unwrap()
    );
    assert!(
        history
            .active_checkpoint_assistant_sequences()
            .await
            .unwrap()
            .contains(&sequence)
    );
    workflow.prepare().unwrap();
    assert_eq!(workflow.artifacts().paths().unwrap(), paths);
    assert!(matches!(
        workflow.complete().unwrap(),
        ModeCompletionOutcome::Completed(_)
    ));
    let (fresh, _) = fixture.workflow();
    assert_eq!(fresh.complete().unwrap(), ModeCompletionOutcome::None);
    fresh.prepare().unwrap();
    assert_ne!(fresh.artifacts().paths().unwrap(), paths);
    assert!(matches!(
        fresh.complete().unwrap(),
        ModeCompletionOutcome::Repair(_)
    ));
    assert_eq!(fs::read_to_string(paths.plan()).unwrap(), "# Plan");
    assert!(
        history
            .active_checkpoint_assistant_sequences()
            .await
            .unwrap()
            .contains(&sequence)
    );
}

#[test]
fn source_text_bom_decoding_is_shared_with_prompt_context() {
    let fixture = Fixture::new();
    let (workflow, parser) = fixture.workflow();
    workflow.prepare().unwrap();
    let paths = workflow.artifacts().paths().unwrap();
    let mut bytes = vec![0xff, 0xfe];
    for unit in " \n# UTF16 plan \n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(paths.plan(), bytes).unwrap();
    fs::write(
        paths.tasks(),
        [b"\xef\xbb\xbf".as_slice(), VALID.as_bytes()].concat(),
    )
    .unwrap();
    parser.answer(VALID, Ok(valid_artifact()));
    let ModeCompletionOutcome::Completed(completed) = workflow.complete().unwrap() else {
        panic!("completion missing")
    };
    assert_eq!(completed.markdown, "# UTF16 plan");
}
