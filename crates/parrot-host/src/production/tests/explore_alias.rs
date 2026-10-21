use super::*;

#[test]
fn production_profile_resolution_aliases_only_selectable_explorer() {
    let mut fixture = Fixture::new();
    let mut configured_explore = fixture.configuration.runtime.profiles["explorer"].clone();
    configured_explore.max_turns = 73;
    fixture
        .configuration
        .runtime
        .profiles
        .insert("explore".into(), configured_explore);
    let profiles = ProductionProfiles::new(&fixture.configuration).unwrap();
    let canonical = profiles.resolve_child("explorer").unwrap();
    let alias = profiles.resolve_child("explore").unwrap();
    assert_eq!(alias.profile.id, "explorer");
    assert_eq!(
        alias.profile.maximum_requests,
        canonical.profile.maximum_requests
    );
    assert!(Arc::ptr_eq(&alias.security, &canonical.security));
    assert!(
        profiles.profiles.contains_key("explore"),
        "configured IDs are retained"
    );
    for invalid in ["Explore", "EXPLORER", "missing-profile"] {
        assert_eq!(
            profiles.resolve_child(invalid).err().unwrap(),
            ChildError::registry(format!("unknown child profile {invalid}"))
        );
    }

    fixture
        .configuration
        .runtime
        .profiles
        .get_mut("explorer")
        .unwrap()
        .is_agent_selectable = false;
    let profiles = ProductionProfiles::new(&fixture.configuration).unwrap();
    let original = profiles.resolve_child("explore").unwrap();
    assert_eq!(original.profile.id, "explore");
    assert_eq!(original.profile.maximum_requests, 73);
    fixture.configuration.runtime.profiles.remove("explorer");
    let profiles = ProductionProfiles::new(&fixture.configuration).unwrap();
    assert_eq!(
        profiles.resolve_child("explore").unwrap().profile.id,
        "explore"
    );
    fixture
        .configuration
        .runtime
        .profiles
        .get_mut("explore")
        .unwrap()
        .is_agent_selectable = false;
    let profiles = ProductionProfiles::new(&fixture.configuration).unwrap();
    assert_eq!(
        profiles.resolve_child("explore").err().unwrap(),
        ChildError::registry("unknown child profile explore")
    );
    std::fs::remove_dir_all(&fixture.root).unwrap();
}

#[tokio::test]
async fn real_production_agent_spawn_accepts_explore_profile_alias() {
    let fixture = Fixture::new();
    let mut replies = std::collections::VecDeque::from([reply(Some((
        "agent_spawn",
        serde_json::json!({
            "name":"alias-child",
            "agent":"explore",
            "model":"local/test",
            "prompt":"finish this local alias regression",
            "fork":"empty"
        }),
    )))]);
    // Root continuation and child execution can request their local replies in either order.
    replies.extend((0..16).map(|_| reply(None)));
    let host = fixture.host_with(Arc::new(ScriptedProvider(Arc::new(Mutex::new(replies)))));
    let session = bounded(host.service().create_session(
        &wire::CreateSessionRequest {
            model: "local/test".into(),
            mode: "build".into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    let mut stream = host
        .service()
        .listen(
            &wire::ListenRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(host.service().send_message(
        &wire::SendMessageRequest {
            user_session_id: session.id,
            text: "spawn the configured explorer using its explore alias".into(),
            delivery: wire::Delivery::DELIVERY_QUEUE.into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    bounded(async {
        let mut child = None;
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            assert!(
                !event.has_turn_failed(),
                "local production turn failed: {event:?}"
            );
            if event.has_agent_started() && event.agent_started().name == "alias-child" {
                child = Some(event.agent_session_id.clone());
            }
            if event.has_agent_finished() && child.as_ref() == Some(&event.agent_session_id) {
                break;
            }
        }
    })
    .await;
    bounded(host.close()).await.unwrap();
    std::fs::remove_dir_all(&fixture.root).unwrap();
}
