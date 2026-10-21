//! Literal image-budget rows from the reviewed original CompactorAndContextTests.
use super::*;
use parrot_provider::sessions::{Message, Role};

#[tokio::test]
async fn original_image_file_size_rows_do_not_change_estimate_or_trigger() {
    let fixture = Fixture::new();
    let provider = Scripted::default();
    let selection = provider.selection(1050000);
    let controller = fixture.controller(fixture.history(), 90);
    for length in [4096, 3 * 1024 * 1024, 5 * 1024 * 1024] {
        let path = fixture.path.join(format!("image-{length}.png"));
        std::fs::write(&path, vec![0; length]).unwrap();
        let message = Message::new(
            Role::User,
            vec![Content::image_file(
                path.to_string_lossy(),
                "image/png",
                1,
                1,
            )],
        );
        let tokens = CompactionPlanner::estimate_tokens(
            &selection.canonical_model,
            std::slice::from_ref(&message),
        )
        .unwrap();
        assert_eq!(tokens, 4104);
        assert!(tokens > 1000);
        let history = vec![message; 4];
        assert_eq!(
            CompactionPlanner::estimate_tokens(&selection.canonical_model, &history).unwrap(),
            4 * tokens
        );
        assert!(
            !controller
                .compactor()
                .planner()
                .estimate_selected_context(&selection, "", &[], &history)
                .unwrap()
                .exceeds_trigger()
        );
    }
    assert!(provider.image_reads.lock().unwrap().is_empty());
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn original_openai_image_context_56_and_150_rows_keep_absolute_trigger_and_unknown_fallback()
{
    let fixture = Fixture::new();
    let provider = Scripted {
        openai_images: true,
        ..Scripted::default()
    };
    let mut selection = provider.selection_model(1050000, "gpt-6-astra");
    Arc::make_mut(&mut selection.routing_snapshot).context_limit =
        Some(ContextSize::parse("250000").unwrap());
    let controller = fixture.controller(fixture.history(), 90);
    let image = Content::image_file("/not-read.png", "image/png", 1200, 1200);
    let message = Message::new(Role::User, vec![image]);
    let history = vec![message.clone(); 56];
    let snapshot = controller
        .compactor()
        .planner()
        .estimate_selected_context(&selection, "", &[], &history)
        .unwrap();
    assert_eq!(snapshot.estimated_tokens - 56 * 8 - 4, 97048);
    assert!(!snapshot.exceeds_trigger());
    assert!(
        controller
            .compactor()
            .planner()
            .estimate_selected_context(&selection, "", &[], &vec![message; 150])
            .unwrap()
            .exceeds_trigger()
    );
    let unknown = provider.selection_model(1050000, "unknown");
    assert_eq!(
        CompactionPlanner::estimate_tokens(&unknown.canonical_model, &history).unwrap() - 56 * 8,
        229376
    );
    let fallback = Scripted::default().selection_model(1050000, "gpt-6-astra");
    assert_eq!(
        CompactionPlanner::estimate_tokens(&fallback.canonical_model, &history).unwrap(),
        CompactionPlanner::estimate_tokens(&unknown.canonical_model, &history).unwrap()
    );
    assert!(provider.image_reads.lock().unwrap().is_empty());
    fixture
        .repository
        .events()
        .database()
        .close()
        .await
        .unwrap();
}

#[tokio::test]
async fn original_openai_image_cost_retention_and_summary_chunk_assertions() {
    let fixture = Fixture::new();
    let history = fixture.history();
    let provider = Scripted {
        openai_images: true,
        ..Scripted::default()
    };
    let selection = provider.selection_model(10000, "gpt-6-astra");
    let path = fixture.path.join("source-shaped-image.png");
    std::fs::write(&path, [1]).unwrap();
    let image = Content::image_file(path.to_string_lossy(), "image/png", 400, 400);
    let messages = vec![Message::new(Role::User, vec![image]); 8];
    let groups = group_messages(&messages);
    let fixed = Message::user("fixed");
    let templates = templates();
    let compactor = Compactor::new(
        CompactionSettings {
            trigger_percent: 90,
            target_percent: 5,
            maximum_input_tokens: 600,
            summary_output_tokens: 100,
            summary_instructions: String::new(),
            summary_prefix: String::new(),
        },
        ModelProfiles::new(&BTreeMap::new()),
        templates,
    )
    .unwrap();
    let mut blobs = CompactionGroupBlobStore::for_session(
        fixture.repository.events().database().selected_root(),
        "scratch",
        Arc::new(Names),
    )
    .unwrap();
    let sessions = ProviderSessions::new();
    let events = Events::new(history);
    let result = compactor
        .compact(
            &selection,
            None,
            &PlanningInput {
                instructions: "",
                tools: &[],
                groups: &groups,
                base_watermark: 0,
                fixed_message: &fixed,
            },
            &mut blobs,
            &sessions,
            &events,
            &NeverCancelled,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.retained_messages, 1);
    assert_eq!(
        result
            .history
            .last()
            .unwrap()
            .contents()
            .iter()
            .filter(|content| matches!(content, Content::Image { .. }))
            .count(),
        1
    );
    assert!(
        CompactionPlanner::estimate_input_tokens(
            &selection.canonical_model,
            "",
            &[],
            &result.history
        )
        .unwrap()
            <= 500
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert!(requests.len() > 1);
    assert!(requests.iter().all(|request| {
        CompactionPlanner::estimate_tokens(&selection.canonical_model, &request.messages).unwrap()
            <= 600
    }));
    assert_eq!(
        requests
            .iter()
            .flat_map(|request| &request.messages)
            .flat_map(|message| message.contents())
            .filter(|content| matches!(content, Content::Image { .. }))
            .count(),
        7
    );
    assert!(
        requests
            .iter()
            .all(|request| request.model == "gpt-6-astra")
    );
    assert_eq!(
        provider.image_reads.lock().unwrap().as_slice(),
        &vec![vec![1]; 7]
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
