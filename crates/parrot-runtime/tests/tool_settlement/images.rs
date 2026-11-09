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
async fn image_budget_latches_remaining_batch_resets_next_batch_and_synthetic_user_is_once() {
    for spare in [0, 1] {
        let fixture = Fixture::new();
        let image = image(&fixture).await;
        let tool = image_tool(&fixture, "image", &image);
        let first = fixture
            .batch(vec![
                call("one", "image"),
                call("overflow", "image"),
                call("latched", "image"),
            ])
            .await;
        let second = fixture.batch(vec![call("next", "image")]).await;
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
        assert!(fixture.history.has_tool_synthetic(first).await.unwrap());
        assert!(fixture.history.has_tool_synthetic(second).await.unwrap());
        let users = fixture
            .history
            .conversation()
            .await
            .unwrap()
            .into_iter()
            .filter(|item| item.message.role == MessageRole::User)
            .collect::<Vec<_>>();
        assert_eq!(users.len(), 2);
        assert!(users.iter().all(|item| item.message.parts.len() == 1));
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
async fn restored_images_are_preserved_and_unfinished_calls_get_crash_errors() {
    for overflow in [false, true] {
        let fixture = Fixture::new();
        let image = image(&fixture).await;
        let sequence = fixture
            .batch(vec![call("restored", "image"), call("pending", "image")])
            .await;
        let terminal = ToolExecutionTerminal {
            tool_call_id: "restored".into(),
            tool_name: "image".into(),
            status: if overflow {
                ToolExecutionStatus::ImageBudgetExceeded
            } else {
                ToolExecutionStatus::Finished
            },
            result_parts: if overflow {
                vec![ConversationPart::text("budget exceeded")]
            } else {
                vec![
                    ConversationPart::text("restored"),
                    ConversationPart::image(image.clone()),
                ]
            },
        };
        let mut event = Event::new();
        event.id = id();
        event.agent_session_id = "agent".into();
        fixture
            .repository
            .events()
            .append_tool_settlement(event, sequence, terminal.clone())
            .await
            .unwrap();
        let tool = image_tool(&fixture, "image", &image);
        let mut harness = Harness::with_limit(
            &fixture,
            vec![tool.clone().handle()],
            image.byte_length as i32,
        )
        .await;
        harness.restore(&fixture).await.unwrap();
        harness.restore(&fixture).await.unwrap();
        let messages = harness.conversation.messages();
        assert!(
            messages
                .iter()
                .filter(|message| message.role == MessageRole::Tool)
                .flat_map(|message| &message.contents)
                .all(|part| matches!(part, parrot_storage::conversation::ModelContent::Text(_)))
        );
        assert_eq!(
            messages
                .iter()
                .flat_map(|message| &message.contents)
                .filter(|part| matches!(
                    part,
                    parrot_storage::conversation::ModelContent::ImageFile { .. }
                ))
                .count(),
            usize::from(!overflow)
        );
        let terminals = fixture
            .history
            .tool_terminals_of_calls(vec!["restored".into(), "pending".into()])
            .await
            .unwrap();
        assert_eq!(terminals[0], terminal);
        assert_eq!(terminals[1].status, ToolExecutionStatus::Error);
        assert!(terminals[1].content().contains(CRASH_MESSAGE));
        assert!(tool.calls.lock().unwrap().is_empty());
        assert_eq!(
            harness
                .publisher
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.has_tool_started())
                .count(),
            0
        );
        assert_eq!(
            fixture
                .history
                .conversation()
                .await
                .unwrap()
                .iter()
                .filter(|item| item.message.role == MessageRole::Tool)
                .count(),
            2
        );
        assert_eq!(
            fixture.history.has_tool_synthetic(sequence).await.unwrap(),
            !overflow
        );
        let events = fixture.events().await;
        assert!(
            !events
                .iter()
                .any(|event| event.has_tool_execution_started())
        );
        assert_eq!(
            events.iter().filter(|event| event.has_tool_error()).count(),
            1
        );
        assert!(
            !harness
                .publisher
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.has_tool_finished()),
            "restoration must not republish old terminal"
        );
    }
}

#[tokio::test]
async fn restored_missing_image_metadata_is_visible_error_and_does_not_advance_watermark() {
    let fixture = Fixture::new();
    let tool = Arc::new(RecordingTool::new(
        &fixture,
        "image",
        ParallelSafety::Sequential,
    ));
    let mut harness = Harness::new(&fixture, vec![tool.clone().handle()]).await;
    let sequence = fixture
        .batch(vec![call("missing", "image"), call("pending", "image")])
        .await;
    let mut event = Event::new();
    event.id = id();
    event.agent_session_id = "agent".into();
    fixture
        .history
        .append_tool_settlement(
            event,
            sequence,
            ToolExecutionTerminal {
                tool_call_id: "missing".into(),
                tool_name: "image".into(),
                status: ToolExecutionStatus::Finished,
                result_parts: vec![
                    ConversationPart::text("image"),
                    ConversationPart::image(ImageArtifactMetadata {
                        artifact_id: "f".repeat(64),
                        sha256: "f".repeat(64),
                        media_type: "image/png".into(),
                        byte_length: 70,
                        width: 1,
                        height: 1,
                        frame_count: 1,
                        aggregate_pixels: 1,
                        display_name: "missing.png".into(),
                        origin: "test".into(),
                    }),
                ],
            },
        )
        .await
        .unwrap();
    let result = harness.restore(&fixture).await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "The image artifact does not exist."
    );
    let pending = fixture
        .history
        .tool_terminals_of_calls(vec!["pending".into()])
        .await
        .unwrap();
    assert_eq!(pending[0].status, ToolExecutionStatus::Error);
    assert!(pending[0].content().contains(CRASH_MESSAGE));
    assert_eq!(harness.state.settled_conversation_sequence(), 0);
    assert!(tool.calls.lock().unwrap().is_empty());
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
