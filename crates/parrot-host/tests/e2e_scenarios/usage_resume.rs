use super::*;

#[tokio::test]
async fn a003_catalog_usage_survives_actual_host_close_and_same_session_resume() {
    let fixture = fixture().await;
    let hosted = fixture.open().await.unwrap();
    let first_root = fixture.root();
    let first_user = fixture.user();
    let id = UserSessionId::parse(&first_user.id).unwrap();
    let first_constructors = fixture.observations.lock().unwrap().constructors;
    let mut query = tool_reply("t76-a003-get", "get_agent_tasks", json!({}));
    query.input_tokens = 10;
    query.cached_input_tokens = 3;
    query.output_tokens = 4;
    let mut stop = reply("t76-a003-first-stop");
    stop.input_tokens = 7;
    stop.cached_input_tokens = 2;
    stop.output_tokens = 5;
    script(&fixture, "t76-a003-input", query, None);
    script(&fixture, "t76-a003-input", stop, None);
    send(&hosted, "t76-a003-input", "t76-a003-first").await;
    idle(&first_root).await;
    let before_close = first_user.lease.events.replay().await.unwrap();
    let first_history = first_root.history().conversation().await.unwrap();
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&first_user);

    let opened = bounded(fixture.store.resume(&id, true, false))
        .await
        .unwrap();
    let resumed = bounded(fixture.factory.bind_opened(opened)).await.unwrap();
    let second_root = fixture.root();
    let second_user = fixture.user();
    let second_constructors = fixture.observations.lock().unwrap().constructors;
    let mut completion = reply("t76-a003-resumed-stop");
    completion.input_tokens = 6;
    completion.cached_input_tokens = 1;
    completion.output_tokens = 2;
    script(&fixture, "t76-a003-resumed-input", completion, None);
    send(&resumed, "t76-a003-resumed-input", "t76-a003-second").await;
    idle(&second_root).await;
    let after_resume = second_user.lease.events.replay().await.unwrap();
    let resumed_history = second_root.history().conversation().await.unwrap();
    let selected_root = second_user.resources.root().to_owned();
    let resumed_id = resumed.snapshot().id.clone();
    let requests = fixture.requests.lock().unwrap().clone();
    bounded(resumed.close()).await.unwrap();
    assert_database_closed(&second_user);

    assert_eq!((first_constructors, second_constructors), (1, 2));
    assert_eq!(resumed_id, id.value());
    assert_eq!(selected_root, first_user.resources.root());
    let terminals = |events: &[Event]| {
        events
            .iter()
            .filter(|event| event.has_turn_ended())
            .map(|event| {
                let terminal = event.turn_ended();
                (terminal.input_tokens, terminal.output_tokens)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(terminals(&before_close), vec![(17, 9)]);
    assert_eq!(terminals(&after_resume), vec![(17, 9), (23, 11)]);
    let facts: Vec<_> = after_resume
        .iter()
        .filter(|event| event.has_request_usage_recorded())
        .map(|event| {
            let fact = event.request_usage_recorded();
            (
                fact.input_tokens,
                fact.cached_input_tokens,
                fact.output_tokens,
            )
        })
        .collect();
    assert_eq!(facts, vec![(10, 3, 4), (7, 2, 5), (6, 1, 2)]);
    let query_result = first_history
        .iter()
        .find(|item| item.message.tool_call_id == "t76-a003-get")
        .unwrap();
    assert_eq!(
        parrot_storage::events::content(&query_result.message.parts),
        "[]"
    );
    assert!(
        resumed_history
            .iter()
            .any(|item| item.message.tool_call_id == "t76-a003-get"
                && item.message == query_result.message)
    );
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "get_agent_tasks")
    );
    assert!(
        requests
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|message| message.tool_call_id == "t76-a003-get")
    );
    assert!(fixture.scripts.lock().unwrap().is_empty());
}
