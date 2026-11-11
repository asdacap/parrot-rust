use super::*;
use parrot_core::ConversationEvent;
use parrot_storage::{
    artifacts::{ImageArtifactRepository, ImageArtifactStore},
    conversation::ConversationPart,
};

#[tokio::test]
async fn provider_template_failure_retains_native_prompt_error_and_terminal_publication() {
    use parrot_runtime::prompt_context::{SystemPromptProvider, TemplateSystemPromptProvider};

    let fixture = Fixture::new();
    let history = fixture.history();
    user(&history, "existing").await;
    let provider = Scripted::default();
    let selection = provider.selection(10000);
    let sessions = ProviderSessions::new();
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let prompt = SessionPrompt::new(
        TemplateSystemPromptProvider::new(
            "test:missing-arguments",
            "context.standing-instruction",
            Vec::new(),
            templates(),
        )
        .materialize(&Default::default())
        .unwrap(),
    );
    let status = Status::default();
    let events = Events::new(history.clone());
    let mut controller = fixture.controller(history, 90);

    let error = controller
        .compact_epoch(
            EpochInput {
                selection: &selection,
                tools: &[],
                conversation: &mut conversation,
                prompt: &prompt,
                sessions: &sessions,
                status: &status,
                events: &events,
            },
            None,
            &NeverCancelled,
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ContextError::Prompt(parrot_runtime::prompt_context::PromptError::Template(_))
    ));
    assert!(provider.requests.lock().unwrap().is_empty());
    {
        let published = events.published.lock().unwrap();
        assert_eq!(published.len(), 2);
        assert!(published[0].has_compaction_started());
        assert!(published[1].has_compaction_failed());
    }
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
async fn status_steering_appends_records_and_publishes_the_newest_standing() {
    let fixture = Fixture::with_identity("agent", "mode");
    let history = fixture.history();
    user(&history, "existing").await;
    let mut conversation = AgentConversation::new(history.clone()).await.unwrap();
    let events = Events::new(history.clone());
    let notice = || ConversationEvent::Notice {
        kind: "status".into(),
        notice: "Status prompt injected".into(),
    };
    let standing = |hash: &str| {
        parrot_core::StatusStanding::from([(parrot_core::StatusKey::new("mode"), hash.to_string())])
    };
    let addition = |content: &str, hash: &str| parrot_core::ConversationAddition {
        messages: vec![parrot_core::Message::text(
            parrot_core::Role::System,
            content,
        )],
        persistence: parrot_core::ConversationPersistence::StatusSteering {
            standing: standing(hash),
            event: notice(),
        },
    };

    parrot_runtime::compaction::apply_conversation_addition(
        &history,
        &mut conversation,
        addition("fitted status", "h1"),
        &events,
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert_eq!(conversation.messages().len(), 2);
    assert_eq!(conversation.messages()[1].content(), "fitted status");
    assert_eq!(
        history.standing_statuses().await.unwrap().unwrap(),
        standing("h1")
    );

    parrot_runtime::compaction::apply_conversation_addition(
        &history,
        &mut conversation,
        addition("next status", "h2"),
        &events,
        &NeverCancelled,
    )
    .await
    .unwrap();
    assert_eq!(conversation.messages().len(), 3);
    assert_eq!(conversation.messages()[2].content(), "next status");
    assert_eq!(
        history.standing_statuses().await.unwrap().unwrap(),
        standing("h2")
    );
    assert_eq!(events.published.lock().unwrap().len(), 2);
    // Project-only status notices are retained broadcasts, not durable Event rows.
    assert!(events.committed.lock().unwrap().is_empty());
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
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn final_request_flag_restores_tools_once_after_projection_commit_and_reopen() {
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
    controller
        .inject_final_provider_request_prompt(
            &mut conversation,
            "duplicate",
            &events,
            &NeverCancelled,
        )
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
        message
            .parts
            .push(ConversationPart::image(artifact.clone()));
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
            tool_group(&history, &["checkpoint", "parallel"]).await;
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
        assert_eq!(emitted.len(), 3);
        assert!(emitted[0].has_compaction_started());
        assert!(emitted[1].has_compaction_progress());
        assert_eq!(emitted[1].compaction_progress().completed_groups, 0);
        assert!(emitted[2].has_compaction_failed());
        assert!(
            !emitted[2]
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
