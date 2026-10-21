use super::*;
use parrot_domain::protocol::messages::parrot::Delivery;
use parrot_storage::{
    artifacts::{ImageArtifactRepository, ImageArtifactStore},
    conversation::ConversationPart,
};

#[tokio::test]
async fn pending_and_initial_status_fit_actual_next_promotion_before_commit_once() {
    for depth in [0, 1] {
        let fixture = Fixture::new();
        let history = fixture.history();
        user(&history, "existing").await;
        let repository = fixture
            .repository
            .clone()
            .with_agent_session_id_generator(Arc::new(|| Ok("agent".into())));
        repository
            .session_state("user".into(), "mode".into())
            .await
            .unwrap();
        history
            .admit_parts(
                "pending-message".into(),
                vec![ConversationPart::text("p".repeat(8000))],
                Delivery::DELIVERY_STEER,
                |_| Ok(event("agent")),
            )
            .await
            .unwrap();
        let provider = Scripted::default();
        let selection = provider.selection(10000);
        let sessions = ProviderSessions::new();
        let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
        let mut prompt = EpochPrompt::default();
        let status = Status::default();
        let events = Events::new(history.clone());
        let mut controller = fixture.controller(history.clone(), 90);
        controller
            .ensure_epoch(&prompt.prompt, &NeverCancelled)
            .await
            .unwrap();
        if depth == 0 {
            assert_eq!(
                controller
                    .inject_status(
                        input(
                            &selection,
                            &mut conversation,
                            &mut prompt,
                            &sessions,
                            &status,
                            &events
                        ),
                        depth,
                        "stale",
                        &NeverCancelled
                    )
                    .await
                    .unwrap(),
                StatusInjection::RefreshSelection
            );
            assert!(status.estimates.lock().unwrap().is_empty());
        }
        let expected = controller
            .compactor()
            .planner()
            .estimate_selected_context(
                &selection,
                "instructions",
                &[],
                &[
                    sessions::Message::user("existing"),
                    sessions::Message::user("p".repeat(8000)),
                    sessions::Message::system(&status.fitted),
                ],
            )
            .unwrap();
        assert_eq!(
            controller
                .inject_status(
                    input(
                        &selection,
                        &mut conversation,
                        &mut prompt,
                        &sessions,
                        &status,
                        &events
                    ),
                    depth,
                    "mode",
                    &NeverCancelled
                )
                .await
                .unwrap(),
            StatusInjection::Complete
        );
        assert_eq!(status.estimates.lock().unwrap().as_slice(), &[expected]);
        assert_eq!(prompt.sampled.load(Ordering::SeqCst), 1);
        assert_eq!(conversation.messages().len(), 2);
        assert_eq!(conversation.messages()[1].content(), status.fitted);
        assert!(history.has_pending_inputs().await.unwrap());
        assert_eq!(
            controller
                .inject_status(
                    input(
                        &selection,
                        &mut conversation,
                        &mut prompt,
                        &sessions,
                        &status,
                        &events
                    ),
                    depth,
                    "mode",
                    &NeverCancelled
                )
                .await
                .unwrap(),
            StatusInjection::Complete
        );
        assert_eq!(conversation.messages().len(), 2);
        assert_eq!(status.estimates.lock().unwrap().len(), 1);
        assert_eq!(events.published.lock().unwrap().len(), 1);
        assert!(events.committed.lock().unwrap().is_empty());
        // Source project-only status notices are retained broadcasts, not durable Event rows.
        assert!(
            !history
                .repository()
                .events()
                .replay()
                .await
                .unwrap()
                .iter()
                .any(|event| event.agent_notice().kind == "status")
        );
        sessions.close().await.unwrap();
        fixture
            .repository
            .events()
            .database()
            .close()
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn candidate_reminder_that_crosses_trigger_compacts_instead_of_persisting_crossing() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "x".repeat(2000)).await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 60);
    controller
        .prepare_epoch(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    let baseline = controller
        .compactor()
        .planner()
        .estimate_selected_context(
            &selection,
            "instructions",
            &[],
            &conversation
                .messages()
                .iter()
                .map(provider_message)
                .collect::<Vec<_>>(),
        )
        .unwrap()
        .estimated_tokens;
    // End just below the strict 60% trigger; the actual rendered reminder will not fit.
    user(&history, "y".repeat(((5999 - baseline - 8) * 4) as usize)).await;
    conversation.reload().await.unwrap();
    let prepared = controller
        .prepare_epoch(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(prepared.reduced);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(history.latest_context_reminder().await.unwrap().is_none());
    assert_eq!(reminder_count(&history).await, 0);
    let after = controller
        .compactor()
        .planner()
        .estimate_selected_context(
            &selection,
            &prepared.instructions,
            &[],
            &conversation
                .messages()
                .iter()
                .map(provider_message)
                .collect::<Vec<_>>(),
        )
        .unwrap();
    assert!(!after.exceeds_trigger());
    assert!(!after.exceeds_input_limit());
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn final_request_marker_restores_tools_once_after_projection_commit_and_reopen() {
    let fixture = Fixture::new();
    let history = fixture.history();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let events = Events::new(history.clone());
    let controller = fixture.controller(history.clone(), 90);
    assert!(
        !controller
            .restore_tool_availability(&mut conversation, "restored", &events, &NeverCancelled)
            .await
            .unwrap()
    );
    controller
        .inject_final_provider_request_prompt(&mut conversation, "final", &events, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(conversation.messages().last().unwrap().content(), "final");
    assert!(
        controller
            .restore_tool_availability(&mut conversation, "restored", &events, &NeverCancelled)
            .await
            .unwrap()
    );
    assert!(
        !controller
            .restore_tool_availability(&mut conversation, "restored", &events, &NeverCancelled)
            .await
            .unwrap()
    );
    assert_eq!(
        conversation
            .messages()
            .iter()
            .map(ModelMessage::content)
            .collect::<Vec<_>>(),
        ["final", "restored"]
    );
    assert!(
        history
            .repository()
            .events()
            .replay()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        events
            .published
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.agent_notice().kind.as_str())
            .collect::<Vec<_>>(),
        [
            "final_provider_request_prompt",
            "tool_availability_restored_prompt"
        ]
    );
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let reopened = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&reopened, "agent");
    let mut restored = AgentConversation::new(history.clone()).await.unwrap();
    let events = Events::new(history.clone());
    let controller = fixture.controller(history, 90);
    assert!(
        !controller
            .restore_tool_availability(&mut restored, "duplicate", &events, &NeverCancelled)
            .await
            .unwrap()
    );
    assert_eq!(restored.messages(), conversation.messages());
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn forced_unbounded_and_precancelled_compaction_make_no_provider_or_event_request() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "old").await;
    user(&history, "tail").await;
    let provider = Scripted::default();
    let selection = provider.selection(0);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    assert!(
        !controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events
                ),
                None,
                &NeverCancelled
            )
            .await
            .unwrap()
            .reduced
    );
    struct Cancelled;
    impl Cancellation for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    assert!(matches!(
        controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events
                ),
                None,
                &Cancelled
            )
            .await
            .unwrap_err(),
        ContextError::Cancelled
    ));
    assert!(events.published.lock().unwrap().is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(
        fixture
            .repository
            .compaction("agent".into())
            .await
            .unwrap()
            .is_none()
    );
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn image_artifacts_are_materialized_loaded_by_summary_provider_and_retained_after_reopen() {
    const PIXEL: &[u8] = include_bytes!("fixtures/pixel.png");
    let fixture = Fixture::new();
    let database = fixture.repository.events().database().clone();
    let store = ImageArtifactStore::for_session(database.selected_root(), "images").unwrap();
    let images = ImageArtifactRepository::new(database, store.clone()).unwrap();
    let artifact = images
        .persist(
            std::io::Cursor::new(PIXEL.to_vec()),
            "upload",
            "pixel.png",
            "test",
            Some("image/png"),
            Some(70),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(
        artifact.sha256,
        "4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5"
    );
    let repository = fixture
        .repository
        .clone()
        .with_image_store(store.clone())
        .unwrap();
    let history = Fixture::history_for(&repository, "agent");
    for text in ["earlier image", "retained image"] {
        let mut message = ConversationMessage::text(MessageRole::User, text);
        message.parts.push(ConversationPart::image(
            &artifact.artifact_id,
            "image/png",
            "pixel.png",
        ));
        append(&history, message).await;
    }
    let provider = Scripted::default();
    let selection = provider.selection(20000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let before = history.conversation().await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    assert!(provider.image_reads.lock().unwrap().is_empty());
    assert!(
        controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events
                ),
                None,
                &NeverCancelled
            )
            .await
            .unwrap()
            .reduced
    );
    assert_eq!(
        provider.image_reads.lock().unwrap().as_slice(),
        &[PIXEL.to_vec()]
    );
    assert_eq!(
        &history.conversation().await.unwrap()[..before.len()],
        before.as_slice()
    );
    let last = provider_message(conversation.messages().last().unwrap());
    assert!(matches!(
        &last.contents()[1],
        Content::Image {
            width: 1,
            height: 1,
            ..
        }
    ));
    assert_eq!(last.contents()[1].read_image().unwrap(), PIXEL);
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let reopened = Fixture::open(&fixture.path)
        .with_image_store(store)
        .unwrap();
    let history = Fixture::history_for(&reopened, "agent");
    assert_eq!(
        AgentConversation::new(history).await.unwrap().messages(),
        conversation.messages()
    );
    sessions.close().await.unwrap();
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn long_committed_history_keeps_the_checkpoint_suffix_and_exact_reopened_tool_results() {
    let fixture = Fixture::new();
    let history = fixture.history();
    for index in 0..200 {
        if index == 150 {
            let assistant = tool_group(&history, &["checkpoint", "parallel"]).await;
            history
                .record_checkpoint("long".into(), assistant, "checkpoint".into())
                .await
                .unwrap();
        }
        user(&history, format!("message {index}: {}", "x".repeat(500))).await;
    }
    let provider = Scripted::default();
    let selection = provider.selection(100000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let retained = conversation.messages()[150..].to_vec();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    let result = controller
        .compact(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            None,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(result.reduced);
    assert_eq!(&conversation.messages()[2..], retained);
    assert_eq!(
        fixture
            .repository
            .compaction("agent".into())
            .await
            .unwrap()
            .unwrap()
            .watermark,
        150
    );
    assert_eq!(provider.requests.lock().unwrap()[0].messages.len(), 151);
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
    let reopened = Fixture::open(&fixture.path);
    let history = Fixture::history_for(&reopened, "agent");
    assert_eq!(
        AgentConversation::new(history).await.unwrap().messages(),
        conversation.messages()
    );
    sessions.close().await.unwrap();
    reopened.events().database().close().await.unwrap();
}

#[tokio::test]
async fn direct_epoch_no_progress_with_nonpositive_input_limit_reports_source_failure() {
    for limit in [0, -10] {
        let fixture = Fixture::new();
        let history = fixture.history();
        user(&history, "tail").await;
        let provider = Scripted::default();
        let selection = provider.selection(limit);
        let sessions = ProviderSessions::new();
        let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
        let mut prompt = EpochPrompt::default();
        let status = Status::default();
        let events = Events::new(history.clone());
        let mut controller = fixture.controller(history.clone(), 90);
        let outcome = controller
            .compact_epoch(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events,
                ),
                None,
                &NeverCancelled,
            )
            .await;
        assert!(
            outcome.is_err(),
            "source CompactEpoch compares estimate directly with input limit {limit}, even when unavailable"
        );
        let error = outcome.unwrap_err();
        assert!(matches!(&error, ContextError::InvalidOperation(_)));
        assert_eq!(
            error.to_string(),
            "The conversation has no safe compaction boundary before the input limit."
        );
        assert!(
            fixture
                .repository
                .compaction("agent".into())
                .await
                .unwrap()
                .is_none()
        );
        let emitted = events.published.lock().unwrap().clone();
        assert_eq!(emitted.len(), 2);
        assert!(emitted[0].has_compaction_started());
        assert!(emitted[1].has_compaction_failed());
        assert!(provider.requests.lock().unwrap().is_empty());
        sessions.close().await.unwrap();
        fixture
            .repository
            .events()
            .database()
            .close()
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn projected_tool_result_estimate_uses_reopened_history_and_exact_template_shape() {
    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "committed").await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let mut prompt = EpochPrompt::default();
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history.clone(), 90);
    let before = controller
        .estimate_context(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert_eq!(prompt.sampled.load(Ordering::SeqCst), 1);
    let invalid = controller
        .estimate_context_after_tool_result(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            " \t",
            "result",
        )
        .await
        .unwrap_err();
    assert!(matches!(&invalid, ContextError::Argument(_)));
    assert_eq!(
        invalid.to_string(),
        "toolCallId must not be empty or whitespace"
    );
    // An in-memory-only candidate must not leak into the source's fresh history probe.
    conversation.add(ModelMessage {
        role: MessageRole::User,
        contents: vec![ModelContent::Text("not durable".repeat(1000))],
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
        provider_content: Default::default(),
    });
    let actual = controller
        .estimate_context_after_tool_result(
            input(
                &selection,
                &mut conversation,
                &mut prompt,
                &sessions,
                &status,
                &events,
            ),
            "tool",
            "result",
        )
        .await
        .unwrap();
    let formatted = templates()
        .render(
            "tool-result.text",
            &[("value".into(), "result".into())],
            &NeverCancelled,
        )
        .unwrap();
    let expected = controller
        .compactor()
        .planner()
        .estimate_selected_context(
            &selection,
            "instructions",
            &[],
            &[
                sessions::Message::user("committed"),
                sessions::Message::tool_result("tool", formatted),
            ],
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert!(actual.estimated_tokens > before.estimated_tokens);
    assert_eq!(history.conversation().await.unwrap().len(), 1);
    assert!(provider.requests.lock().unwrap().is_empty());
    sessions.close().await.unwrap();
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn provider_failure_keeps_snapshot_absent_budget_calibration_and_top_level_body_only() {
    for wrapped in [false, true] {
        let fixture = Fixture::new();
        let history = fixture.history();
        user(&history, "old").await;
        user(&history, "tail").await;
        let provider = Scripted::default();
        let inner = Error::Response(
            parrot_provider::responses::ResponsesError::ProviderResponse {
                error_type: "type".into(),
                error_code: "code".into(),
                detail: "failed summary".into(),
                response_body: "private-response-body".into(),
            },
        );
        *provider.failure.lock().unwrap() = Some(if wrapped {
            Error::ProviderWithCause {
                message: "wrapped failure".into(),
                cause: Box::new(inner),
            }
        } else {
            inner
        });
        let selection = provider.selection(10000);
        let sessions = ProviderSessions::new();
        let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
        let before = conversation.messages().to_vec();
        let mut prompt = EpochPrompt::default();
        let status = Status::default();
        let events = Events::new(history.clone());
        let mut controller = fixture.controller(history.clone(), 90);
        controller.budget_mut().observe_usage(
            "scripted/model",
            100,
            &Completion {
                input_tokens: 1000,
                ..Completion::default()
            },
        );
        let error = controller
            .compact(
                input(
                    &selection,
                    &mut conversation,
                    &mut prompt,
                    &sessions,
                    &status,
                    &events,
                ),
                None,
                &NeverCancelled,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.response_body(),
            if wrapped { "" } else { "private-response-body" }
        );
        assert_eq!(
            controller
                .budget()
                .estimate_input_tokens("scripted/model", 100),
            1000
        );
        assert!(
            fixture
                .repository
                .compaction("agent".into())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(conversation.messages(), before);
        let emitted = events.published.lock().unwrap().clone();
        assert_eq!(emitted.len(), 2);
        assert!(emitted[0].has_compaction_started());
        assert!(emitted[1].has_compaction_failed());
        assert!(
            !emitted[1]
                .compaction_failed()
                .message
                .contains("private-response-body")
        );
        drop(emitted);
        assert_eq!(provider.closed.load(Ordering::Relaxed), 1);
        sessions.close().await.unwrap();
        fixture
            .repository
            .events()
            .database()
            .close()
            .await
            .unwrap();
    }
}
