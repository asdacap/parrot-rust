use super::*;
use parrot_storage::artifacts::{ImageArtifactRepository, ImageArtifactStore};

pub(super) async fn image(fixture: &Fixture) -> ImageArtifactMetadata {
    let root = fixture.repository.events().database().selected_root();
    ImageArtifactRepository::new(
        fixture.repository.events().database().clone(),
        ImageArtifactStore::for_session(root, "resources").unwrap(),
    )
    .unwrap()
    .persist(
        std::io::Cursor::new(include_bytes!("fixtures/pixel.png").to_vec()),
        "pixel.png",
        "T47 synthetic fixture",
        Some("image/png"),
        None,
        &NeverCancelled,
    )
    .await
    .unwrap()
}
fn image_tool(fixture: &Fixture, name: &str, image: &ImageArtifactMetadata) -> Arc<RecordingTool> {
    let mut tool = RecordingTool::new(fixture, name, ParallelSafety::Sequential);
    tool.output.image_artifacts = vec![image.clone()];
    Arc::new(tool)
}
#[tokio::test]
async fn image_budget_latches_remaining_batch_and_only_successes_post_steers() {
    for spare in [0, 1] {
        let fixture = Fixture::new();
        let image = image(&fixture).await;
        let tool = image_tool(&fixture, "image", &image);
        fixture
            .batch(vec![
                call("one", "image"),
                call("overflow", "image"),
                call("latched", "image"),
            ])
            .await;
        fixture.batch(vec![call("next", "image")]).await;
        let mut harness = Harness::with_limit(
            &fixture,
            vec![tool.clone().handle()],
            image.byte_length as i32 + spare,
        )
        .await;
        harness.run(&fixture, &NeverCancelled).await.unwrap();
        harness.run(&fixture, &NeverCancelled).await.unwrap();
        let terminals = fixture
            .history
            .tool_terminals_of_calls(vec![
                "one".into(),
                "overflow".into(),
                "latched".into(),
                "next".into(),
            ])
            .await
            .unwrap();
        assert_eq!(
            terminals
                .iter()
                .map(|terminal| terminal.status)
                .collect::<Vec<_>>(),
            vec![
                ToolExecutionStatus::Finished,
                ToolExecutionStatus::ImageBudgetExceeded,
                ToolExecutionStatus::ImageBudgetExceeded,
                ToolExecutionStatus::Finished
            ]
        );
        assert_eq!(tool.calls.lock().unwrap().len(), 4);

        let users = fixture
            .history
            .conversation()
            .await
            .unwrap()
            .into_iter()
            .filter(|item| item.message.role == MessageRole::User)
            .collect::<Vec<_>>();
        assert!(users.is_empty());
        let promoted = fixture
            .history
            .promote_steers(|input| {
                let mut event = Event {
                    id: id(),
                    agent_session_id: "agent".into(),
                    ..Event::new()
                };
                event.set_input_promoted(protocol::InputPromoted {
                    input_id: input.id.clone(),
                    message_id: input.message_id.clone(),
                    ..Default::default()
                });
                Ok(event)
            })
            .await
            .unwrap();
        assert_eq!(promoted.len(), 2);
        for promotion in promoted {
            assert_eq!(
                promotion.input.parts,
                [
                    ConversationPart::text(&image.artifact_id),
                    ConversationPart::image(image.clone())
                ]
            );
        }
        assert_eq!(
            fixture
                .events()
                .await
                .iter()
                .filter(|event| event.has_tool_finished())
                .count(),
            2
        );
    }
}

#[tokio::test]
async fn completed_tool_recovery_does_not_reconstruct_missing_image_steers() {
    let fixture = Fixture::new();
    let image = image(&fixture).await;
    let sequence = fixture.batch(vec![call("completed", "image")]).await;
    fixture
        .history
        .append_tool_settlement(
            Event {
                id: id(),
                agent_session_id: "agent".into(),
                ..Event::new()
            },
            sequence,
            ToolExecutionTerminal {
                tool_call_id: "completed".into(),
                tool_name: "image".into(),
                status: ToolExecutionStatus::Finished,
                result_parts: vec![ConversationPart::text(&image.artifact_id)],
            },
        )
        .await
        .unwrap();
    let mut harness = Harness::new(&fixture, vec![]).await;
    harness.restore(&fixture).await.unwrap();
    assert!(
        fixture
            .history
            .peek_pending_input()
            .await
            .unwrap()
            .is_none()
    );
    assert!(harness.state.read_recovery().await.unwrap().is_none());
    assert_eq!(fixture.history.conversation().await.unwrap().len(), 2);
}

#[tokio::test]
async fn yielded_process_metadata_is_preserved_on_finished_event_and_result_projection() {
    let fixture = Fixture::new();
    let mut tool = RecordingTool::new(&fixture, "process", ParallelSafety::Sequential);
    let mut yielded = protocol::YieldedShellProcess::new();
    yielded.process_id = "owned-process".into();
    yielded.name = "named process".into();
    yielded.inventory_instance_id = "inventory-instance".into();
    yielded.visible_revision = u64::MAX;
    yielded.stdout_path = Some("scratch/process.stdout".into());
    yielded.stderr_path = Some("scratch/process.stderr".into());
    yielded.activity_kind = protobuf::EnumOrUnknown::from_i32(-37);
    yielded
        .special_fields
        .mut_unknown_fields()
        .add_varint(127, 12345);
    tool.output.yielded_process = Some(yielded.clone());
    let tool = Arc::new(tool);
    fixture.batch(vec![call("yielded", "process")]).await;
    let mut harness = Harness::new(&fixture, vec![tool.handle()]).await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    let events = fixture.events().await;
    let finished = events
        .iter()
        .find(|event| event.has_tool_finished())
        .unwrap()
        .tool_finished();
    assert_eq!(finished.yielded_process.as_ref(), Some(&yielded));
    assert_eq!(
        harness
            .diagnostics
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .outcome
            .as_deref(),
        Some("yielded")
    );
    assert_eq!(
        fixture
            .history
            .tool_terminals_of_calls(vec!["yielded".into()])
            .await
            .unwrap()[0]
            .status,
        ToolExecutionStatus::Finished
    );
}

#[tokio::test]
async fn image_reads_post_pending_steers_after_all_tool_results() {
    let fixture = Fixture::new();
    let image = image(&fixture).await;
    let mut reader = RecordingTool::new(&fixture, "image", ParallelSafety::Sequential);
    reader.output.text = image.artifact_id.clone();
    reader.output.image_artifacts = vec![image.clone()];
    let reader = Arc::new(reader);
    let text = Arc::new(RecordingTool::new(
        &fixture,
        "text",
        ParallelSafety::Sequential,
    ));
    fixture
        .batch(vec![
            call("first", "image"),
            call("second", "text"),
            call("third", "image"),
        ])
        .await;
    let mut harness = Harness::new(&fixture, vec![reader.handle(), text.handle()]).await;
    harness.run(&fixture, &NeverCancelled).await.unwrap();
    let history = fixture.history.conversation().await.unwrap();
    assert_eq!(
        history
            .iter()
            .map(|item| item.message.role)
            .collect::<Vec<_>>(),
        [
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::Tool
        ]
    );
    let pending = fixture
        .history
        .peek_pending_input()
        .await
        .unwrap()
        .expect("image steer is pending");
    assert_eq!(pending.delivery, protocol::Delivery::DELIVERY_STEER);
    assert_eq!(
        pending.parts,
        [
            ConversationPart::text(&image.artifact_id),
            ConversationPart::image(image.clone())
        ]
    );
    let promotions = fixture
        .history
        .promote_steers(|input| {
            let mut event = Event {
                id: id(),
                agent_session_id: "agent".into(),
                ..Event::new()
            };
            event.set_input_promoted(protocol::InputPromoted {
                input_id: input.id.clone(),
                message_id: input.message_id.clone(),
                ..Default::default()
            });
            Ok(event)
        })
        .await
        .unwrap();
    assert_eq!(promotions.len(), 2);
    let history = fixture.history.conversation().await.unwrap();
    assert_eq!(
        history
            .iter()
            .map(|item| item.message.role)
            .collect::<Vec<_>>(),
        [
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::User,
            MessageRole::User
        ]
    );
    for item in [&history[1], &history[3]] {
        assert_eq!(
            item.message.parts,
            [ConversationPart::text(&image.artifact_id)]
        );
    }
    for item in &history[4..] {
        assert_eq!(
            item.message.parts,
            [
                ConversationPart::text(&image.artifact_id),
                ConversationPart::image(image.clone())
            ]
        );
        let materialized = fixture
            .history
            .materialize(item.message.parts.clone())
            .await
            .unwrap();
        assert!(matches!(
            &materialized[1],
            parrot_storage::conversation::ModelContent::ImageFile { .. }
        ));
    }
    assert!(
        fixture
            .history
            .peek_pending_input()
            .await
            .unwrap()
            .is_none()
    );
}
